//! Request and response accounting, by part of Puna and by room.
//!
//! One fairing over both web roles, exporting the three families `puna_core::metrics` declares:
//! requests, request bytes, response bytes, labeled `{kind, room}`.
//!
//! ## Both labels are cardinality decisions
//!
//! A public listener gets scanned, so **no label value here is ever taken from the request as
//! written**. `kind` comes from [`kind_of`], whose `match` arms *are* its vocabulary, so the label
//! space is fixed at compile time however creative a URL is.
//!
//! `room` is harder, because the useful value is an id somebody could invent. Two sources, and the
//! rule that keeps both bounded:
//!
//! * **A handler that resolved a room says so**, through [`RoomTag`]. That is the only way the
//!   tracker and the feed can be attributed at all, since `/tracker/<id>` and `/journal/<id>` carry
//!   ids that are deliberately not the room's, and it is the strongest source: the row exists,
//!   because it was read.
//! * **Otherwise the path, and only if the response was not an error.** `/room/<id>` names its own
//!   room, and a `2xx` or a `3xx` is the server saying it did something for that id. A random uuid
//!   answers `404` (an unknown room, tracker or feed id all do, which is a property this codebase
//!   already relies on elsewhere) and is therefore never labeled, so a scan mints no series.
//!
//! Anything else is `room=""`, which is a real value rather than a placeholder: most of what this
//! serves is not about one room.
//!
//! ## What it costs
//!
//! Series are `kinds x rooms-actually-served x 3`, per replica, and they accumulate until the
//! process restarts: a room that stops existing keeps its series until then, since this tier has no
//! fleet loop to reconcile against (`retain_rooms`, the orchestrator's answer to the same problem,
//! is driven by the reconcile tick). Bounded by real rooms rather than by traffic, which is the
//! point of everything above.

use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use prometheus::IntCounter;
use puna_core::ids::RoomId;
use puna_core::metrics::{HTTP_REQUEST_BYTES, HTTP_REQUESTS, HTTP_RESPONSE_BYTES};
use rocket::Data;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::response::{Body, Response};
use rocket::tokio::io::{AsyncRead, ReadBuf};
use rocket::{Request, http::uri::Origin};

/// Where a handler puts the room it resolved, for the fairing to label with.
///
/// **Write-once**, so the label cannot change under a request that has already been decided, and so
/// a handler that resolves twice cannot make the second answer win.
///
/// Reached as a request guard (`tag: &RoomTag`), which is what makes it a *parameter* of the two
/// functions that resolve an independent id. That is deliberate: adding it to `access` and
/// `readable` made the compiler visit all nine of their call sites, where a helper somebody had to
/// remember to call would have been forgotten on the tenth.
#[derive(Default)]
pub struct RoomTag(OnceLock<RoomId>);

impl RoomTag {
    /// Name the room this request turned out to be about. Later calls are ignored.
    pub fn set(&self, room: RoomId) {
        let _ = self.0.set(room);
    }

    fn get(&self) -> Option<RoomId> {
        self.0.get().copied()
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for &'r RoomTag {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(request.local_cache(RoomTag::default))
    }
}

/// Which part of Puna a path belongs to.
///
/// Pure, and takes the path rather than the request, so the whole vocabulary is testable without a
/// server. Everything unrecognized is `other` by construction rather than by omission.
pub fn kind_of(path: &str) -> &'static str {
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    match segments.next() {
        Some("generations") => "generations",
        // The listing, the creation POST and everything under one room. The `room` label is what
        // separates "a room" from "your rooms" inside this kind.
        Some("room" | "rooms") => "room",
        Some("journal") => "journal",
        Some("tracker") => "tracker",
        // The reference-compatible JSON lives under `/api/tracker` and `/api/static_tracker`, and
        // Puna's own digested views under `/api/puna/tracker`. All three are the tracker.
        Some("api") => match segments.next() {
            Some("tracker" | "static_tracker") => "tracker",
            Some("puna") if segments.next() == Some("tracker") => "tracker",
            _ => "other",
        },
        Some("static") => "static",
        Some("health" | "readyz") => "health",
        _ => "other",
    }
}

/// The room a request was about, as a label value.
///
/// See the module docs for why the path is trusted only behind a non-error status, and the tag
/// unconditionally.
fn room_of(request: &Request<'_>, status: Status) -> String {
    if let Some(room) = request.local_cache(RoomTag::default).get() {
        return room.to_string();
    }
    if status.code >= 400 {
        return String::new();
    }
    room_in_path(request.uri())
}

/// `/room/<id>/...` and nothing else. A uuid anywhere else in a path is not a room.
fn room_in_path(uri: &Origin<'_>) -> String {
    let path = uri.path();
    let mut segments = path.as_str().split('/').filter(|s| !s.is_empty());
    if segments.next() != Some("room") {
        return String::new();
    }
    match segments.next() {
        // Parsed rather than passed through, so the label is a uuid or nothing.
        Some(id) => id
            .parse::<RoomId>()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// The counter a room's feed socket adds its frames to.
///
/// The WebSocket's traffic never passes through a response body, so the fairing cannot see it: an
/// upgrade is one request whose body is empty and whose bytes all flow afterwards. The feed's own
/// send sites add to this instead, into the same family, because "how much did we send for this
/// room's history" has one answer and the page is a rounding error beside the socket.
///
/// **Resolved once and held, then incremented per frame.** Two reasons, and the second is the one
/// that decides it. `with_label_values` is a hash and a lock, which is the wrong thing to do per
/// frame on a busy feed. And a frame must be counted **when it is sent**: a feed connection lives
/// for hours, so anything that waited for the end would lose the lot every time the tier is
/// redeployed, which disconnects every viewer at once. A counter that only reports on connections
/// that ended cleanly under-reports exactly when there is most to report.
pub fn feed_bytes(room: RoomId) -> IntCounter {
    HTTP_RESPONSE_BYTES.with_label_values(&["journal", &room.to_string()])
}

pub struct HttpMetrics;

#[rocket::async_trait]
impl Fairing for HttpMetrics {
    fn info(&self) -> Info {
        Info {
            name: "HTTP metrics",
            kind: Kind::Request | Kind::Response,
        }
    }

    /// **Request bytes are counted here, when the request arrives, not when it is answered.**
    ///
    /// The number is `Content-Length`, which is known before a byte of the body is read, and
    /// counting it now is what makes it survive: the one request whose body is big enough for the
    /// timing to matter is a generation upload, which is tens or hundreds of megabytes over as many
    /// seconds, and a tier replaced mid-upload would otherwise report nothing at all for it.
    ///
    /// **So request bytes never carry a room**, and that is the trade rather than an oversight: the
    /// room is not knowable until the response (see `room_of`), and taking it from the path here
    /// would let anyone with a `curl` mint a series per invented uuid. Nothing is lost by it, since
    /// every large body Puna accepts is roomless by nature and every room-scoped body is a form.
    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        if let Some(bytes) = request
            .headers()
            .get_one("Content-Length")
            .and_then(|value| value.parse::<u64>().ok())
        {
            HTTP_REQUEST_BYTES
                .with_label_values(&[kind_of(request.uri().path().as_str()), ""])
                .inc_by(bytes);
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let kind = kind_of(request.uri().path().as_str());
        let room = room_of(request, response.status());
        let labels = [kind, room.as_str()];

        HTTP_REQUESTS.with_label_values(&labels).inc();

        match response.body().preset_size() {
            // Already known, so nothing is wrapped and nothing about the response changes. Counted
            // in full here rather than as it goes: these are the small responses (a page, a JSON
            // document), and the alternative would mean re-setting a sized body, which costs its
            // `Content-Length`. A client that disconnects mid-page is over-counted by the tail it
            // did not receive, which at this size is not a number anybody is reading.
            Some(bytes) => {
                HTTP_RESPONSE_BYTES
                    .with_label_values(&labels)
                    .inc_by(bytes as u64);
            }
            // A streamed body, which is how the two largest things this serves go out: a patch and
            // a gzipped journal are both handed over as they are produced rather than buffered.
            //
            // **Wrapped only here, where the framing is already chunked.** Re-setting a sized body
            // as a streamed one would drop its `Content-Length` and switch it to chunked, which
            // would be a real change to every response in exchange for a number already in hand.
            None => {
                let body = response.body_mut().take();
                response.set_streamed_body(Counted {
                    inner: Box::pin(body),
                    sent: HTTP_RESPONSE_BYTES.with_label_values(&labels),
                });
            }
        }
    }
}

/// A response body that counts what it hands over, **as it hands it over**.
///
/// Not on drop and not at end-of-stream, for the reason [`feed_bytes`] gives at greater length: a
/// journal download is up to 250 MB and a patch is tens, so anything that published at the end
/// would lose a whole transfer to a pod being replaced, and would report nothing at all about one
/// still in flight. Each poll adds what that poll produced, so the counter is true continuously and
/// a client that vanishes half way is counted for the half it got.
///
/// The child counter is resolved once, when the body is wrapped, so the per-poll cost is one atomic
/// add rather than a hash and a lock.
struct Counted<'r> {
    inner: Pin<Box<Body<'r>>>,
    sent: IntCounter,
}

impl AsyncRead for Counted<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = self.inner.as_mut().poll_read(cx, buf);
        // Only what this poll actually put in the buffer. `filled` is cumulative for the caller's
        // buffer, so the difference is the read's own contribution.
        let read = buf.filled().len().saturating_sub(before);
        if read > 0 {
            self.sent.inc_by(read as u64);
        }
        polled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::local::blocking::Client;

    /// The counter's value for one label pair, or zero.
    fn requests(kind: &str, room: &str) -> u64 {
        HTTP_REQUESTS.with_label_values(&[kind, room]).get()
    }

    fn bytes(kind: &str, room: &str) -> u64 {
        HTTP_RESPONSE_BYTES.with_label_values(&[kind, room]).get()
    }

    // A room that exists, and one nobody will ever create. Fixed rather than random, since the
    // assertions below are about which of the two appears as a label.
    const REAL: &str = "0189aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa";
    const INVENTED: &str = "0189bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb";

    #[rocket::get("/room/<id>")]
    fn room(id: &str) -> Result<&'static str, Status> {
        // Standing in for every resolution in this codebase: an id that names nothing answers 404,
        // which is what the room label's whole safety rests on.
        if id == REAL {
            Ok("a room page")
        } else {
            Err(Status::NotFound)
        }
    }

    #[rocket::get("/tracker/<_id>")]
    fn tracker(_id: &str, tag: &RoomTag) -> &'static str {
        // A tracker id is not a room's, so the handler names the room it resolved, exactly as
        // `routes::tracker::access` does.
        tag.set(REAL.parse().expect("a room id"));
        "a tracker page"
    }

    /// **A scan cannot mint series, and a resolved id is attributed anyway.**
    ///
    /// The two halves of the room label, end to end through a real router rather than through
    /// `room_of` alone: the whole point is what happens to a request nobody authorized, and that is
    /// a property of the fairing plus the response, not of a function.
    ///
    /// Invented ids are the failure that would not look like one. Every series here is cheap and
    /// permanent, so a crawler walking `/room/<uuid>` would grow the registry without bound while
    /// every page on every dashboard carried on working, until a scrape started timing out.
    #[test]
    fn an_invented_room_id_is_never_a_label_and_a_resolved_one_always_is() {
        let rocket = rocket::build()
            .attach(HttpMetrics)
            .mount("/", rocket::routes![room, tracker]);
        let client = Client::tracked(rocket).expect("a test client");

        let before = (requests("room", INVENTED), requests("room", REAL));

        assert_eq!(
            client.get(format!("/room/{INVENTED}")).dispatch().status(),
            Status::NotFound
        );
        assert_eq!(
            requests("room", INVENTED),
            before.0,
            "a room id that names nothing became a label, so anybody with a uuid generator can \
             grow this registry until the scrape falls over"
        );
        assert_eq!(
            requests("room", ""),
            1,
            "the refused request was not counted at all; it is real traffic and belongs under the \
             kind with no room"
        );

        assert!(
            client
                .get(format!("/room/{REAL}"))
                .dispatch()
                .status()
                .class()
                .is_success()
        );
        assert_eq!(
            requests("room", REAL),
            before.1 + 1,
            "a room that answered is not attributed to itself"
        );
        assert!(
            bytes("room", REAL) > 0,
            "a page was served and no response bytes were counted for it"
        );

        // And the id that is deliberately not a room's: only the handler can say what it resolved.
        assert_eq!(
            client.get(format!("/tracker/{REAL}")).dispatch().status(),
            Status::Ok
        );
        assert_eq!(
            requests("tracker", REAL),
            1,
            "a tracker request carried no room, so the tier's whole traffic is one unattributed \
             bucket"
        );
    }

    /// **Every path lands in the vocabulary, and the vocabulary is what the registry seeds.**
    ///
    /// The two halves fail differently and both are silent. A kind this returns that no role seeds
    /// is a series that appears out of nowhere on first use, which is survivable; a kind that is
    /// seeded and never returned is a permanent zero that reads as "nobody uses this", which is
    /// the failure the per-role seeding exists to avoid in the first place.
    #[test]
    fn every_kind_is_reachable_and_every_answer_is_a_kind() {
        let cases = [
            ("/generations", "generations"),
            ("/generations/new", "generations"),
            (
                "/generations/018f0000-0000-7000-8000-000000000000",
                "generations",
            ),
            ("/rooms", "room"),
            ("/room/018f0000-0000-7000-8000-000000000000", "room"),
            (
                "/room/018f0000-0000-7000-8000-000000000000/slot/3/patch",
                "room",
            ),
            ("/journal/018f0000-0000-7000-8000-000000000000", "journal"),
            (
                "/journal/018f0000-0000-7000-8000-000000000000/feed",
                "journal",
            ),
            ("/tracker/018f0000-0000-7000-8000-000000000000", "tracker"),
            (
                "/tracker/018f0000-0000-7000-8000-000000000000/0/3",
                "tracker",
            ),
            (
                "/api/tracker/018f0000-0000-7000-8000-000000000000",
                "tracker",
            ),
            (
                "/api/static_tracker/018f0000-0000-7000-8000-000000000000",
                "tracker",
            ),
            (
                "/api/puna/tracker/018f0000-0000-7000-8000-000000000000/slots",
                "tracker",
            ),
            ("/static/css/puna.css", "static"),
            ("/health", "health"),
            ("/readyz", "health"),
            ("/", "other"),
            ("/admin/rooms", "other"),
            ("/auth/login", "other"),
            ("/api/v1/generations", "other"),
            ("/claim/abcdef", "other"),
        ];

        for (path, expected) in cases {
            assert_eq!(kind_of(path), expected, "{path}");
        }

        // Nothing invents a kind the registry does not know about.
        for (path, _) in cases {
            assert!(
                puna_core::metrics::HTTP_KINDS.contains(&kind_of(path)),
                "{path} answers a kind that is not in HTTP_KINDS"
            );
        }

        // And every kind either role seeds is one some path can actually produce, so no seeded zero
        // is permanent by construction.
        for kind in puna_core::metrics::WEB_HTTP_KINDS
            .iter()
            .chain(puna_core::metrics::TRACKER_HTTP_KINDS)
        {
            assert!(
                cases.iter().any(|(path, _)| kind_of(path) == *kind),
                "`{kind}` is seeded and no path in this table reaches it"
            );
        }
    }

    /// **A scan must not be able to mint series.** The path is trusted only behind a response that
    /// says the id named something, and an unknown room, tracker or feed id answers `404`.
    #[test]
    fn a_path_room_is_taken_only_from_a_response_that_is_not_an_error() {
        let real = "/room/018f0000-0000-7000-8000-000000000000";
        let uri = Origin::parse(real).expect("a path");
        assert_eq!(
            room_in_path(&uri),
            "018f0000-0000-7000-8000-000000000000",
            "a room path does not yield its own id"
        );

        // Not a uuid, so not a label however the request is answered.
        let junk = Origin::parse("/room/..%2F..%2Fetc").expect("a path");
        assert_eq!(room_in_path(&junk), "");

        // A uuid somewhere that is not a room's path segment.
        let tracker =
            Origin::parse("/tracker/018f0000-0000-7000-8000-000000000000").expect("a path");
        assert_eq!(
            room_in_path(&tracker),
            "",
            "a tracker id was read as a room id, which both leaks and mislabels"
        );
    }
}
