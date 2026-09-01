//! A response fairing that puts `Secure` on every cookie leaving this process.
//!
//! ## Why a fairing rather than setting it where each cookie is built
//!
//! `Session::save` does set it explicitly, and should. But Puna is not the only thing that writes
//! a cookie here: `rocket_oauth2` sets `rocket_oauth2_state` (the OAuth CSRF token) from inside
//! [`OAuth2::get_redirect`], built as `Cookie::build(..).same_site(Lax).build()` with no `Secure`
//! and **no configuration hook to add one**. There is nothing to pass and nothing to override.
//!
//! So the guarantee has to live somewhere that sees cookies Puna did not construct. This is that
//! place, and it covers whatever a future dependency adds without anyone having to notice.
//!
//! ## Why the attribute is missing in the first place
//!
//! Rocket only defaults `Secure` on when Rocket itself terminates TLS: `CookieJar::set_defaults`
//! is `if cookie.secure().is_none() && config.tls_enabled()`. Every Puna deployment terminates TLS
//! upstream at Envoy, so `tls_enabled()` is false and no cookie ever gets the attribute by
//! default. The reasonable assumption that Rocket handles this is exactly what makes it invisible:
//! nothing warns, and the cookies look correct in every other respect.
//!
//! ## Why it matters more here than in a typical app
//!
//! The UI and the multiworld rooms **share a hostname**, differing only by port, and cookies have
//! no port isolation: a cookie scoped to `rooms.example.com` is sent to `rooms.example.com:41234`,
//! a pahoa room, exactly as it is to `:443`. pahoa neither parses nor logs cookies and the values
//! are AEAD-encrypted, so it cannot read them. But they are bearer credentials: `punasession`
//! replays a login, and `rocket_oauth2_state` is what stops an attacker completing an OAuth flow
//! on someone else's behalf. Without `Secure`, one plaintext request to any room port puts them on
//! the wire in the clear, and there is no HSTS on the shared gateway to upgrade that request.
//!
//! ## Unconditional, deliberately
//!
//! It is not gated on the request being HTTPS. Behind a TLS-terminating proxy the process sees
//! plain HTTP on every request, so a condition would be false exactly when it matters. Local
//! development over `http://localhost` still works, because browsers treat localhost as a secure
//! context and accept `Secure` cookies there; developing against a bare LAN address would break
//! login, which is the same trade `Session::save` already makes.

use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::Header;
use rocket::{Request, Response};

pub struct SecureCookies;

#[rocket::async_trait]
impl Fairing for SecureCookies {
    fn info(&self) -> Info {
        Info {
            name: "Secure attribute on every cookie",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, _request: &'r Request<'_>, response: &mut Response<'r>) {
        let existing: Vec<String> = response
            .headers()
            .get("Set-Cookie")
            .map(String::from)
            .collect();

        // The common case by far: no cookies at all. Touching the header map otherwise would mean
        // a remove-and-readd on every static asset and every poll of /room/<id>/status.
        if existing.iter().all(|cookie| has_secure(cookie)) {
            return;
        }

        // `Set-Cookie` legitimately repeats, and there is no edit-in-place for one occurrence, so
        // the whole set is rebuilt. Order within the set does not matter to a browser.
        response.remove_header("Set-Cookie");
        for cookie in existing {
            if has_secure(&cookie) {
                response.adjoin_header(Header::new("Set-Cookie", cookie));
            } else {
                response.adjoin_header(Header::new("Set-Cookie", format!("{cookie}; Secure")));
            }
        }
    }
}

/// Whether a `Set-Cookie` value already carries the `Secure` attribute.
///
/// **`skip(1)` is load-bearing**: the first `;`-separated segment is `name=value`, and a cookie
/// whose VALUE happens to be or contain `secure` must not be read as carrying the attribute. Our
/// own values are base64 ciphertext, so this is defensive rather than observed, but the failure
/// it prevents is a credential silently shipped without the flag, which is not a failure that
/// announces itself.
fn has_secure(header: &str) -> bool {
    header
        .split(';')
        .skip(1)
        .any(|attribute| attribute.trim().eq_ignore_ascii_case("secure"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::http::Status;
    use rocket::local::blocking::Client;

    #[test]
    fn the_attribute_is_detected_only_as_an_attribute() {
        assert!(has_secure("a=b; Secure"));
        assert!(has_secure("a=b; secure")); // attributes are case-insensitive
        assert!(has_secure("a=b; Path=/; Secure; HttpOnly"));
        assert!(has_secure("a=b; Secure ")); // trailing whitespace

        assert!(!has_secure("a=b"));
        assert!(!has_secure("a=b; Path=/; HttpOnly"));

        // The reason for skip(1): a value that looks like the attribute is still a value.
        assert!(!has_secure("session=secure"));
        assert!(!has_secure("secure=1; Path=/"));
        // And an attribute merely CONTAINING the word is not the attribute.
        assert!(!has_secure("a=b; SecureFlag"));
    }

    #[rocket::get("/one")]
    fn one() -> (Status, ()) {
        (Status::Ok, ())
    }

    /// The end-to-end property, asserted against a real response rather than against `has_secure`.
    ///
    /// Two cookies on one response, mimicking the login redirect: one already `Secure` (as
    /// `Session::save` builds it) and one without (as `rocket_oauth2` builds its state cookie).
    /// Both must come out `Secure`, and the one that already had it must not be duplicated.
    #[test]
    fn every_set_cookie_leaves_with_the_attribute() {
        struct AddCookies;

        #[rocket::async_trait]
        impl Fairing for AddCookies {
            fn info(&self) -> Info {
                Info {
                    name: "test cookies",
                    kind: Kind::Response,
                }
            }
            async fn on_response<'r>(&self, _req: &'r Request<'_>, res: &mut Response<'r>) {
                res.adjoin_header(Header::new("Set-Cookie", "punasession=abc; Secure; Path=/"));
                res.adjoin_header(Header::new("Set-Cookie", "rocket_oauth2_state=xyz; Path=/"));
            }
        }

        // SecureCookies is attached SECOND so it runs after the cookies exist. Rocket runs
        // response fairings in attach order, which is the same reason it goes on last in `build`.
        let rocket = rocket::build()
            .mount("/", rocket::routes![one])
            .attach(AddCookies)
            .attach(SecureCookies);
        let client = Client::untracked(rocket).expect("a client");
        let response = client.get("/one").dispatch();

        let cookies: Vec<&str> = response.headers().get("Set-Cookie").collect();
        assert_eq!(
            cookies.len(),
            2,
            "cookies were lost or duplicated: {cookies:?}"
        );
        for cookie in &cookies {
            assert!(has_secure(cookie), "not marked Secure: {cookie}");
            assert_eq!(
                cookie.matches("Secure").count(),
                1,
                "Secure appended to a cookie that already had it: {cookie}"
            );
        }
    }
}
