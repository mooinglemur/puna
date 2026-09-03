//! A lint over the template sources.
//!
//! `askama.toml` sets `whitespace = "suppress"`, which strips whitespace adjacent to every tag. The
//! escape is `{{+ ... }}`, and it **preserves** whitespace rather than inserting any, so
//! `as {{+ name }}` is right and `as{{+ name }}` renders `asTroy`.
//!
//! The second form looks like it does the same thing, and on 2026-08-20 **every** use of `{{+` in
//! this crate was that shape: the space had been deleted at the same time the `+` was added. It
//! reached production on the home page. Nothing about it is visible in a diff or a compile, and a
//! render test only catches the one string it happens to assert, so the guard is a lint over the
//! sources, which catches every instance including ones nobody has written yet.

use std::path::{Path, PathBuf};

fn templates() -> Vec<PathBuf> {
    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read the templates directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|e| e == "html") {
                into.push(path);
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let mut found = Vec::new();
    walk(&root, &mut found);
    assert!(!found.is_empty(), "no templates found under {root:?}");
    found
}

/// `{{+` with no whitespace in front of it preserves nothing, so it is always a mistake: either the
/// space belongs there, or the `+` does not.
#[test]
fn a_whitespace_preserving_tag_has_whitespace_to_preserve() {
    let mut offenders = Vec::new();

    for path in templates() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
        // Comments are blanked rather than skipped by line, because `base.html` documents the wrong
        // form ON PURPOSE and a comment spans lines. Blanking preserves line numbers, so the ones
        // reported below still point at the real file.
        let source = blank_comments(&raw);

        for (number, line) in source.lines().enumerate() {
            for (at, _) in line.match_indices("{{+").chain(line.match_indices("{%+")) {
                let preceding = line[..at].chars().next_back();
                match preceding {
                    // Start of line: the newline before it is the whitespace being preserved.
                    None => {}
                    Some(c) if c.is_whitespace() => {}
                    Some(c) => offenders.push(format!(
                        "{}:{}: `{}{{{{+` preserves nothing: add the space or drop the `+`",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        number + 1,
                        c
                    )),
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "whitespace-preserving tags with no whitespace to preserve:\n  {}",
        offenders.join("\n  ")
    );
}

/// The other half of the same trap: a space between rendered text and a tag is **eaten**.
///
/// The first lint catches `word{{+ x }}`, where the `+` preserves nothing. This catches the case
/// with no `+` at all: `{{ count }} rooms` renders `4rooms`, and `running {% if %}` renders
/// `runningsha-abc`. Both shipped: the drift label and the configured-image line on
/// `/admin/rooms`, within an hour of each other, by someone who had just fixed the first kind.
///
/// **A render test cannot cover this**, which is why it is a source lint. It only catches the exact
/// string it asserts, and these are one-word joins scattered through markup nobody re-reads.
///
/// The rule: whitespace adjacent to a tag survives only with an explicit `+` on that side. So this
/// flags whitespace with **rendered text on both sides** of it, which is the only case where the
/// loss is visible. Whitespace between two tags (`{% endif %}\n{% if %}`) is not flagged: nothing
/// renders there. Nor is whitespace against markup (`>\n{% if %}`, `{{ x }}\n<td>`), where HTML
/// collapses it anyway.
///
/// **A newline in the run means it is not flagged either**, and that exclusion is what makes this
/// usable rather than noisy. The common harmless shape is a branch whose whole body is on its own
/// line:
///
/// ```text
/// {% else if row.complete() %}
/// cached
/// {% else %}
/// ```
///
/// Here the stripped whitespace is layout, the word is the entire cell, and nothing joins to
/// anything. A run with no newline is different in kind: it is a space someone typed *between two
/// words on one line*, and losing it is always visible. Twelve of the former and fourteen of the
/// latter existed when this lint was written; only the latter were bugs.
#[test]
fn whitespace_between_text_and_a_tag_is_preserved_explicitly() {
    let mut offenders = Vec::new();

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        // The last two components, because there are two `show.html` files and a bare basename
        // sent me to the wrong one while writing this.
        let name = label(&path);

        for (at, opener) in tag_positions(&source) {
            // Text, then whitespace, then this tag: the whitespace before it is eaten.
            let before = source[..at].trim_end_matches([' ', '\t', '\n', '\r']);
            let run = &source[before.len()..at];
            let preserved = source[at..].starts_with(&format!("{opener}+"));

            if run.is_empty() || preserved || !before.ends_with(is_rendered_text) {
                continue;
            }
            // Whitespace before a block terminator is trailing content rather than a word join:
            // nothing follows it inside the branch for the text to run together with.
            if run.contains('\n')
                && (terminates_a_block(&source[at..])
                    || (!continues_a_sentence(before)
                        && !line_containing(&source, before.len() - 1).contains('<')))
            {
                continue;
            }
            if let Some(line) = line_of(&source, at) {
                offenders.push(format!(
                    "{name}:{line}: text before `{opener}` loses its space: write `{opener}+`"
                ));
            }
        }

        for (end, closer) in close_positions(&source) {
            let after = source[end..].trim_start_matches([' ', '\t', '\n', '\r']);
            let text_at = source.len() - after.len();
            let run = &source[end..text_at];
            // `+` rides INSIDE the closer (`+}}`), so it is the character before it, not after.
            let preserved = source[..end - closer.len()].ends_with('+');

            if run.is_empty() || preserved || !after.starts_with(is_rendered_text) {
                continue;
            }
            if run.contains('\n') && !line_containing(&source, text_at).contains('<') {
                continue;
            }
            if let Some(line) = line_of(&source, end) {
                offenders.push(format!(
                    "{name}:{line}: text after `{closer}` loses its space: write `+{closer}`"
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "whitespace adjacent to a tag is stripped, so these render run together:\n  {}",
        offenders.join("\n  ")
    );
}

/// A control with no words carries **both** `title` and `aria-label`, which are not alternatives.
///
/// `aria-label` names the control for assistive technology and renders nothing on screen; `title`
/// is the hover tooltip a pointer user gets. A button whose entire content is an `<svg>` shows no
/// words at all, so with only the first it is a picture nobody can identify by pointing at it.
///
/// **This was reported twice.** M19b gave the four glyph controls in `rooms/show.html` a `title`
/// after the gap was noticed there; `rooms/panel.html`'s address copy button was missed and was
/// reported the same way the next day. Neither was visible to anything else here: the markup is
/// valid, the attribute that *is* present is spelled correctly, and the control works.
#[test]
fn a_glyph_only_control_names_itself_twice() {
    let mut offenders = Vec::new();
    let mut examined = 0;

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        let name = label(&path);

        // `summary` belongs here with the other two: `<details>` is this codebase's default toggle
        // because it needs no script, so a glyph-only summary is an ordinary control rather than an
        // exotic one: the room page's rename pencil is exactly that.
        for element in ["button", "a", "summary"] {
            for (at, open, content) in elements(&source, element) {
                if renders_text(content) {
                    continue;
                }
                // **Not a control for anybody, so not this lint's business.** A form's default
                // button (the one that claims what Enter does, so a row's remove button cannot)
                // is never painted, never announced and never reachable by tab. Both conditions
                // are required together on purpose: `aria-hidden` alone would be a way to silence
                // this lint on a control people can still see and press.
                if open.contains("aria-hidden=\"true\"") && open.contains("tabindex=\"-1\"") {
                    continue;
                }
                examined += 1;
                for attribute in ["title=", "aria-label="] {
                    if !open.contains(attribute) {
                        let line = line_of(&source, at).unwrap_or_default();
                        offenders.push(format!(
                            "{name}:{line}: <{element}> renders no words and has no `{attribute}`"
                        ));
                    }
                }
            }
        }
    }

    // A source lint is the easiest kind to write vacuously, so say how much it must have seen.
    // Twenty-nine glyph controls exist today: twenty-seven until the roster gained a mention copy
    // and the members page an invite copy. The moderation column is why this number moves in steps,
    // and it once moved DOWN:
    // release and collect went into an overflow menu and gained written labels, so they are no
    // longer glyph-only and no longer this lint's business, while the menu's own button is. A
    // change that leaves none is a change this lint stopped guarding. Set it by reading the count
    // this assertion prints, not by guessing.
    assert!(
        examined >= 29,
        "only {examined} glyph-only controls found: this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a control with no words needs a hover tooltip AND an accessible name:\n  {}",
        offenders.join("\n  ")
    );
}

/// Every `<name ...>...</name>` in the source, as its offset, its opening tag and its content.
///
/// No element here nests inside another of its own kind, so matching the next closing tag is the
/// whole job.
fn elements<'a>(source: &'a str, name: &str) -> Vec<(usize, &'a str, &'a str)> {
    let (open, close) = (format!("<{name}"), format!("</{name}>"));
    let mut found = Vec::new();
    let mut i = 0;

    while let Some(offset) = source[i..].find(&open) {
        let at = i + offset;
        i = at + open.len();
        // `<a` must not match `<abbr`, so the name has to end where the tag says it does.
        if !matches!(source[i..].chars().next(), Some(c) if c.is_whitespace() || c == '>') {
            continue;
        }
        let Some(tag_end) = source[at..].find('>').map(|n| at + n + 1) else {
            break;
        };
        let Some(end) = source[tag_end..].find(&close).map(|n| tag_end + n) else {
            break;
        };
        found.push((at, &source[at..tag_end], &source[tag_end..end]));
        i = end + close.len();
    }

    found
}

/// Whether an element's content puts any words on screen.
///
/// An `<svg>` is dropped whole (it *is* the glyph, not a label for it), as are markup tags and
/// control-flow tags, which render nothing themselves. An expression `{{ ... }}` counts as text,
/// because whatever it interpolates is something the reader can see and read the control by.
fn renders_text(content: &str) -> bool {
    let mut rendered = String::new();
    let mut i = 0;

    while i < content.len() {
        let rest = &content[i..];
        if rest.starts_with("<svg") {
            i += rest
                .find("</svg>")
                .map_or(rest.len(), |n| n + "</svg>".len());
        } else if rest.starts_with("{%") {
            i += rest.find("%}").map_or(rest.len(), |n| n + 2);
        } else if rest.starts_with("{{") {
            rendered.push('x');
            i += rest.find("}}").map_or(rest.len(), |n| n + 2);
        } else if rest.starts_with('<') {
            i += rest.find('>').map_or(rest.len(), |n| n + 1);
        } else {
            let c = rest.chars().next().expect("a character");
            rendered.push(c);
            i += c.len_utf8();
        }
    }

    !rendered.trim().is_empty()
}

/// Replace every `{% ... %}` span with spaces, so what is left is what the template renders.
///
/// Indices survive, which is the point: the callers compare positions inside the original string.
fn blank_tags(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < source.len() {
        if source[i..].starts_with("{%") {
            let end = source[i..]
                .find("%}")
                .map_or(source.len(), |offset| i + offset + 2);
            out.extend(
                source[i..end]
                    .chars()
                    .map(|c| if c == '\n' { '\n' } else { ' ' }),
            );
            i = end;
        } else {
            let c = source[i..].chars().next().expect("in bounds");
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Every `{{ ... }}` in a string, as `(offset, the trimmed expression)`.
///
/// The `+` whitespace markers are stripped, so `{{+ room_name +}}` reads as `room_name`: a caller
/// asking *which value is this* should not have to know how its spacing was spelled.
fn expressions(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source.match_indices("{{").filter_map(|(at, _)| {
        let rest = &source[at + 2..];
        let end = rest.find("}}")?;
        Some((at, rest[..end].trim_matches(['+', ' ', '\t'])))
    })
}

/// Content that actually renders, as opposed to markup or another tag.
///
/// `>` and `<` are excluded because HTML collapses whitespace around a tag boundary anyway, and
/// `{`/`}` because whitespace between two template tags renders nothing either way.
///
/// Whitespace itself is excluded too, and leaving it out was the bug in this lint's first draft:
/// the runs are trimmed of spaces and tabs only, so a tag indented on its own line leaves a `\n`
/// as the neighboring character. Counting that as text flagged every indented `{% if %}` in the
/// crate.
fn is_rendered_text(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '>' | '<' | '{' | '}')
}

/// Is this text a sentence carrying on, rather than the whole of a branch's body?
///
/// **The newline exclusion above is what let a real bug through**, and this is the narrowing.
/// `... it could match. {{ ... }} it could not.` split across two lines rendered as
/// *"it could not.They still have their claim links"*, flowing prose in a `<p>`, where the
/// exclusion's whole argument is that the stripped whitespace is layout around a branch whose body
/// is a word on a line of its own.
///
/// The tell is the punctuation. A word standing in for a table cell does not end in `,` or `.`;
/// prose that continues onto the next line does, and there the space is the one between two
/// sentences. It only has to hold for text with a NEWLINE before the tag: everything else is
/// flagged already.
fn continues_a_sentence(before: &str) -> bool {
    before.ends_with(['.', ',', ';', ':', '!', '?'])
}

fn line_of(source: &str, at: usize) -> Option<usize> {
    Some(source.get(..at)?.matches('\n').count() + 1)
}

/// The whole source line containing `at`.
///
/// Used to tell a prose line running into a tag (`<p>Rooms should be running` + newline + `{% if
/// %}`) from a branch body sitting alone on its line (`cached`). The first joins to whatever the
/// tag emits; the second is the entire cell and joins to nothing.
fn line_containing(source: &str, at: usize) -> &str {
    let start = source[..at].rfind('\n').map_or(0, |n| n + 1);
    let end = source[at..].find('\n').map_or(source.len(), |n| at + n);
    &source[start..end]
}

/// `{% endif %}`, `{% else %}` and friends: tags that close or divide a block.
fn terminates_a_block(tag: &str) -> bool {
    let body = tag
        .trim_start_matches(['{', '%', '+'])
        .trim_start()
        .trim_start_matches('-')
        .trim_start();
    body.starts_with("end") || body.starts_with("else") || body.starts_with("when")
}

/// `tracker/show.html` rather than `show.html`. There are two of each name under `templates/`, and
/// a bare basename points at whichever one the reader assumes.
fn label(path: &Path) -> String {
    let mut parts: Vec<String> = path
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.reverse();
    parts.join("/")
}

fn tag_positions(source: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for opener in ["{{", "{%"] {
        found.extend(source.match_indices(opener).map(|(at, _)| (at, opener)));
    }
    found
}

/// Byte offsets just past each closing delimiter.
fn close_positions(source: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for closer in ["}}", "%}"] {
        found.extend(
            source
                .match_indices(closer)
                .map(|(at, _)| (at + closer.len(), closer)),
        );
    }
    found
}

/// Replace every `{# ... #}` span with spaces, keeping newlines so line numbers survive.
///
/// Askama comments do not nest, so a scan for the next `#}` is the whole job.
fn blank_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        if source[i..].starts_with("{#") {
            let end = source[i..]
                .find("#}")
                .map_or(bytes.len(), |offset| i + offset + 2);
            for c in source[i..end].chars() {
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            i = end;
        } else {
            let c = source[i..].chars().next().expect("a character");
            out.push(c);
            i += c.len_utf8();
        }
    }

    out
}

/// **The theme selector's contract, which spans three files and fails silently in all of them.**
///
/// Choosing a theme is one property: `theme.js` writes `data-theme` on `<html>`, and `puna.css`
/// turns that into `color-scheme`, which every `light-dark()` token resolves against. The three
/// buttons name their choice in `data-set`.
///
/// Break any half of that and **the control keeps working**: the button still highlights, the
/// choice still persists across a reload, and the page never changes color. Nothing errors, nothing
/// logs, and the only way to notice is to look at it, which is how a theme switcher gets shipped
/// broken and stays that way.
///
/// So each side is read out of its own file and checked against the others.
#[test]
fn the_theme_selector_agrees_across_markup_script_and_stylesheet() {
    let script = std::fs::read_to_string(source("static/theme.js")).expect("theme.js");
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");
    let base = std::fs::read_to_string(source("templates/base.html")).expect("base.html");

    // The attribute the script writes is the attribute the stylesheet keys on. Written as
    // `dataset.theme` in JavaScript and `[data-theme=` in CSS, so neither spelling can be grepped
    // for in the other file, which is exactly why this drifts unnoticed.
    assert!(
        script.contains("documentElement.dataset.theme"),
        "theme.js no longer writes the attribute the stylesheet reads"
    );

    for choice in ["light", "dark"] {
        assert!(
            css.contains(&format!(
                ":root[data-theme=\"{choice}\"] {{ color-scheme: {choice}; }}"
            )),
            "puna.css does not turn data-theme={choice} into a color-scheme, so choosing it \
             would change nothing"
        );
        assert!(
            base.contains(&format!("data-set=\"{choice}\"")),
            "no button offers {choice}"
        );
    }

    // Following the system is the third state and is the ABSENCE of a stored value, so the
    // stylesheet marks it active with `:not([data-theme])` rather than a value of its own. A
    // `[data-theme="system"]` rule would never match anything the script writes.
    assert!(
        base.contains("data-set=\"system\""),
        "no button offers following the system"
    );
    assert!(
        css.contains(":root:not([data-theme]) .theme button[data-set=\"system\"]"),
        "nothing marks the follow-the-system button as the active one"
    );
    assert!(
        !css.contains("[data-theme=\"system\"]"),
        "`system` is the absence of the attribute, so a rule keyed on that value is dead"
    );

    // The bare `:root` must keep `light dark`: a reader who has chosen nothing follows their
    // system, which this page has done since M10 and which the override must not take away.
    assert!(
        css.contains("color-scheme: light dark;"),
        "the default stopped following the system"
    );

    // Revealed by a class, because without scripting the control cannot work at all.
    assert!(script.contains("classList.add(\"js-theme\")"));
    assert!(css.contains(".theme { display: none; }") && css.contains(".js-theme .theme {"));

    // **Not deferred.** A deferred script runs after parsing, so the page would paint in the system
    // theme and snap to the chosen one: a white flash on every navigation for somebody who picked
    // dark. This is the assertion that keeps somebody from "tidying" it in with the others.
    //
    // Anchored on `<script` AND the filename together, not on the filename alone: the comment
    // above the tag explains why it is not deferred and therefore *mentions* `static/theme.js`, so
    // a search for the name finds prose first and this passes with the tag mutated. It did, until
    // a mutation caught it. Same shape as the dispatcher's ordering lint.
    let tag = base
        .lines()
        .find(|line| line.contains("<script") && line.contains("theme.js"))
        .expect("base.html loads theme.js with a script tag");
    assert!(
        !tag.contains("defer") && !tag.contains("async"),
        "theme.js must block, or the theme arrives after the first paint: {tag}"
    );
}

/// A path inside the crate, for reading a source file a test asserts against.
fn source(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// **Viewing the site as somebody else must be read-only, and this asserts WHERE that is decided.**
///
/// The rule lives in the `Session` request guard rather than in `LoggedInSession`, and that is not
/// a stylistic choice: `POST /room/<id>/start` takes a plain `Session`, because D8 lets an
/// anonymous visitor start an idle room. A check one rung up would leave exactly that route open,
/// and the symptom would be a room started by somebody who did not start it, invisible in the
/// audit trail, which would name the person being viewed.
///
/// Asserted over the source because there is no way to assert "no route was forgotten" from a
/// test that exercises routes: the property is about the guard every other guard composes on.
#[test]
fn viewing_as_somebody_is_read_only_at_the_base_guard() {
    // **Comments stripped first.** Prose about a rule contains the rule's own identifiers: the
    // `LoggedInSession` guard's comment explains why it does not call `from_request_sync`, and
    // naming it there was enough to trip the negative assertion below. Third time in this codebase
    // that a source lint has matched its own explanation; see `puna-silent-breakage`.
    let auth = code_only(&std::fs::read_to_string(source("src/auth.rs")).expect("auth.rs"));

    // The refusal, inside the `Session` impl rather than any other. Split there so a copy of this
    // check living only in `LoggedInSession` cannot satisfy it.
    let base = auth
        .split_once("impl<'r> FromRequest<'r> for Session {")
        .expect("the Session guard exists")
        .1
        .split_once("\n}")
        .expect("it ends")
        .0;

    assert!(
        base.contains("view_as.is_some()") && base.contains("Method::Get"),
        "the base session guard no longer refuses writes while viewing as somebody: a write route \
         taking a plain `Session` (POST /room/<id>/start does) would be reachable"
    );

    // `LoggedInSession` must go THROUGH that guard rather than around it. Calling the sync
    // constructor here instead would quietly exempt every authenticated write route from the rule
    // above, and nothing else would look different.
    let logged_in = auth
        .split_once("impl<'r> FromRequest<'r> for LoggedInSession {")
        .expect("the LoggedInSession guard exists")
        .1
        .split_once("\n}")
        .expect("it ends")
        .0;
    assert!(
        logged_in.contains("request.guard::<Session>()"),
        "LoggedInSession bypasses the Session guard, so the read-only rule does not reach it"
    );
    assert!(
        !logged_in.contains("from_request_sync"),
        "LoggedInSession reads the cookie directly, which skips the read-only refusal"
    );

    // Exactly one route may write while impersonating, and it is the way out. It takes no session
    // guard at all (it could not be reached through one), so the thing to assert is that it
    // remains the ONLY caller of the bypass.
    let users = code_only(
        &std::fs::read_to_string(source("src/routes/users.rs")).expect("routes/users.rs"),
    );
    assert!(
        users.contains("Session::from_cookies(cookies)"),
        "stop-view-as no longer reads the cookie directly, so it cannot work while impersonating"
    );

    let bypass: usize = ["src/auth.rs", "src/routes/users.rs", "src/routes/rooms.rs"]
        .iter()
        .map(|f| {
            code_only(&std::fs::read_to_string(source(f)).unwrap_or_default())
                .matches("from_cookies(")
                .count()
        })
        .sum();
    assert!(
        bypass <= 3,
        "`Session::from_cookies` has grown callers: it skips the read-only refusal, and every use \
         outside stop-view-as needs justifying ({bypass} found)"
    );

    // And an impersonated session must not carry admin rights: seeing admin-only affordances
    // through somebody else's eyes is the opposite of what the feature answers.
    assert!(
        users.contains("is_admin: false"),
        "the impersonated session keeps its admin flag, so it does not show what that user sees"
    );
}

/// Rust source with `//` line comments removed, keeping newlines so the shape is unchanged.
///
/// For lints that assert something about *code*: a comment explaining a rule names the identifiers
/// the rule is about, so an assertion over raw source reads the explanation as though it were the
/// thing explained. That has produced a vacuous pass twice here and a false failure once.
fn code_only(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// **Saving the restart form only rewrites the password mode when it actually changed.**
///
/// `room::set_slot_auth` regenerates every slot password on its way into `per_slot`, correctly,
/// since that is what switching modes means. So re-submitting the mode a room is already in rotates
/// the lot, invalidating a password every player is holding, on a form somebody pressed to change
/// the remote-admin checkbox beside it.
///
/// **Nothing else can catch it.** The route returns success, the page renders the same values, and
/// the damage surfaces later as players who cannot connect with the password they were given. The
/// guard is one comparison, and a source lint is the only thing that notices if it goes.
#[test]
fn the_password_mode_is_only_rewritten_when_it_changed() {
    let source = std::fs::read_to_string(source("src/routes/rooms.rs")).expect("rooms.rs");
    let code = code_only(&source);
    let route = code
        .split_once("async fn set_restart_options(")
        .expect("the restart route")
        .1;
    let body = route.split_once("\n}\n").map_or(route, |(b, _)| b);

    assert!(
        body.contains("mode != access.room.slot_auth"),
        "the restart form rewrites the password mode without comparing it to the room's, so \
         pressing Save rotates every slot password whether or not the mode was touched"
    );
    let compared = body
        .find("mode != access.room.slot_auth")
        .expect("the guard");
    let applied = body.find("set_slot_auth").expect("the write");
    assert!(
        compared < applied,
        "the mode is written before it is compared, so the comparison guards nothing"
    );
}

/// **A link's `source` is shown, and never shown as the identity.**
///
/// The three link records carry two names. `team`/`slot`/`player` come off the authenticated
/// connection the `Bounce` arrived on; `source` is copied straight out of the payload: the sending
/// client's own claim, with nothing in the protocol stopping one from naming somebody else. pahoa
/// records them separately *because they can disagree*, and pins that they can.
///
/// **Both halves of this are load-bearing, and the first draft got the second one wrong by dropping
/// `source` entirely.** It is not noise: one slot can be a whole group of people (Archipelago's
/// Minecraft world puts several accounts behind a single server holding the slot), so `source` is
/// the only field saying which of them died, and withholding it drops the one fact the room cannot
/// otherwise report.
///
/// What must not happen is `source` filling the identity cell, where a name an attacker picks would
/// read as the room's answer. That mistake never shows up in testing, because the two agree for
/// every honest client, which is all of them until one is not.
///
/// So the lint pins the shape rather than the field: `who()` is the identity and reads only the
/// authenticated name, and `source` reaches the page through `claimed()`, dimmed and carrying a
/// `title` that says where it came from.
#[test]
fn a_links_claimed_sender_is_shown_but_never_as_the_identity() {
    let script = std::fs::read_to_string(source("static/journal.js")).expect("journal.js");
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");
    let code = code_only(&script);

    let who = code
        .split_once("function who(row, event) {")
        .expect("who()")
        .1;
    let who = who.split_once("\n  }").expect("a closed who()").0;
    assert!(
        !who.contains("source"),
        "the identity cell reads `source`, which is a name the sending client chose for itself"
    );

    // It is still rendered, through the one helper that marks it as a claim.
    assert!(
        code.contains("function claimed(row, event)") && code.contains("\"claimed\""),
        "nothing renders a link's `source`, so a slot shared by several people cannot say which \
         of them the event was about"
    );
    assert!(
        code.contains("event.source"),
        "`claimed()` no longer reads `source`"
    );
    // Dimmed and footnoted rather than styled like a verified name: the whole point is that it
    // must not read as authority.
    assert!(
        styles(&css, "claimed"),
        "`.claimed` has no rule, so a client-supplied name renders identically to the room's own"
    );
    let claimed = code
        .split_once("function claimed(row, event) {")
        .expect("claimed()")
        .1;
    let claimed = claimed.split_once("\n  }").expect("a closed claimed()").0;
    assert!(
        claimed.contains(".title ="),
        "`claimed()` renders an unverified name with nothing saying it is unverified"
    );

    // Every link type offers it. Missing one is silent: the field simply never appears for that
    // convention, on the records where it is most likely to differ.
    for kind in ["deathlink", "traplink", "ringlink"] {
        let arm = code
            .split_once(&format!("case \"{kind}\":"))
            .unwrap_or_else(|| panic!("a `{kind}` arm"))
            .1;
        let arm = arm.split_once("break;").expect("a terminated arm").0;
        assert!(
            arm.contains("claimed(row, event)"),
            "`{kind}` does not render the sender the client reported"
        );
    }
}

/// **Every record type the public feed carries has a renderer.**
///
/// `PUBLIC_KINDS` decides what reaches a viewer at the `feed` tier, and `journal.js` decides what
/// that viewer *sees*. They are two lists in two languages with nothing tying them together, and
/// the failure mode is one-directional and ugly: a kind admitted by the filter with no `case` in
/// the renderer falls through to the raw-JSON default, so the general public gets a wall of
/// `{"type":"traplink","at":1787159859.507,…}` where the feed should be.
///
/// That is not hypothetical: `deathlink` had no renderer for as long as it was withheld, and
/// admitting it is what made the gap visible. The default exists for a type this build has never
/// heard of; a type it deliberately publishes is not that.
#[test]
fn every_publicly_visible_record_has_a_renderer() {
    let script = std::fs::read_to_string(source("static/journal.js")).expect("journal.js");
    let code = code_only(&script);
    let routes = std::fs::read_to_string(source("src/routes/journal.rs")).expect("journal.rs");

    // Past the `=` before looking for the `;`: the type annotation is `[&str; N]`, so a naive
    // "everything up to the first semicolon" reads the declaration's own length and finds no
    // strings at all. The floor assertion below is what caught that, which is the entire reason a
    // lint that scans for a list states a minimum.
    let list = routes
        .split_once("pub const PUBLIC_KINDS")
        .expect("PUBLIC_KINDS")
        .1;
    let list = list.split_once('=').expect("an initializer").1;
    let list = list.split_once(';').expect("a terminated list").0;
    let kinds: Vec<&str> = list
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|k| !k.is_empty())
        .collect();

    assert!(
        kinds.len() >= 5,
        "read {} public kinds out of PUBLIC_KINDS, so this lint is checking almost nothing",
        kinds.len()
    );
    for kind in kinds {
        assert!(
            code.contains(&format!("case \"{kind}\":")),
            "`{kind}` is sent to a public viewer and `journal.js` has no case for it, so it \
             renders as raw JSON to everybody holding the feed link"
        );
    }
}

/// **The whole-feed walk clears its in-flight flag before asking for the next page.**
///
/// `askForEarlier` refuses to send while `backfilling` is set: one request in flight at a time,
/// so a slow disk backs the walk up instead of queueing a thousand backwards seeks. The walk then
/// continues itself from the frame handler, which means the handler must clear the flag *before*
/// calling it. It did not: the flag was cleared only in the arm taken when the walk had already
/// reached the start of the file, so every continuation was rejected by the guard.
///
/// **The result was a silent stop after one page.** On a room with 160,000 records the button
/// loaded 5,000, disabled itself, and left the note reading "Loading earlier records…" forever.
/// Nothing threw, so there was nothing in a console, and the failure is indistinguishable from a
/// short file, which is why it was reported as "it seems to load a chunk, but then it stops".
///
/// Pinned here because nothing in the Rust build parses this file, and the bug is one line of
/// ordering rather than anything a renderer or a route could notice. Verified against the real
/// script under a stubbed DOM: 32 pages and 160,000 records with the clear in place, 1 page and
/// 5,000 without it.
#[test]
fn the_whole_feed_walk_can_take_more_than_one_page() {
    let source = std::fs::read_to_string(source("static/journal.js")).expect("journal.js");
    let branch = source
        .split_once(r#"if (frame.kind === "earlier")"#)
        .expect("the backfill branch")
        .1;
    let branch = branch.split_once("\n      }").map_or(branch, |(b, _)| b);

    let cleared = branch.find("backfilling = false").expect(
        "the backfill branch never clears `backfilling`, so the walk stops after its first page",
    );
    let continues = branch
        .find("askForEarlier()")
        .expect("the backfill branch never continues the walk");
    assert!(
        cleared < continues,
        "`backfilling` is cleared after the walk asks for its next page, so `askForEarlier` \
         refuses it and the whole-feed button loads exactly one page, silently"
    );
}

/// **The feed's markup, script and stylesheet agree about their hooks.**
///
/// Three files, three spellings of the same contract: `journal.html` names the elements,
/// `journal.js` looks them up by id and paints classes onto what it builds, and `puna.css` gives
/// those classes meaning. Every half of it fails silently: a renamed id makes the script return at
/// its first `if`, so the page renders and simply never connects, with nothing in a console anybody
/// has open; a class the stylesheet does not know leaves the feed as undifferentiated grey text,
/// which reads as a design choice rather than a bug.
///
/// The item classes are the sharpest case: they encode the protocol's `flags`, so losing one means
/// a trap and a progression item look identical to somebody scanning for what just hit them.
#[test]
fn the_journal_feed_agrees_across_markup_script_and_stylesheet() {
    let markup = std::fs::read_to_string(source("templates/rooms/journal.html")).expect("template");
    let script = std::fs::read_to_string(source("static/journal.js")).expect("journal.js");
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");
    let code = code_only(&script);

    // Ids the script gives up on, silently, if the template renames them.
    //
    // `journal-earlier` and `journal-progress` are the backfill control: `journal.js` guards on
    // their absence, so a rename does not throw. The button simply never appears and the whole
    // feed can never be loaded, with nothing anywhere saying why.
    for id in [
        "journal",
        "journal-status",
        // The status paragraph is now a container: a dot and a message, as two spans. `say` writes
        // to the MESSAGE, so if it ever went back to writing the paragraph's `textContent` the dot
        // would be deleted on the first status change, which is to say immediately and forever.
        "journal-message",
        "journal-link",
        "journal-earlier",
        "journal-progress",
    ] {
        assert!(
            code.contains(&format!("getElementById(\"{id}\")")),
            "journal.js no longer looks up `{id}`"
        );
        assert!(
            markup.contains(&format!("id=\"{id}\"")),
            "the journal template does not render `#{id}`, so the feed never starts"
        );
    }

    // The FEED's id travels through a data attribute, and the script returns without it.
    //
    // **`data-feed`, never `data-room`.** The page is addressed by an id that is not the room's, so
    // the room's id must not appear in its markup at all: putting it in a data attribute would leak
    // it just as surely as an `href` would, to every viewer holding a link meant to be shareable.
    assert!(
        !markup.contains("data-room="),
        "the feed page carries the room's id in a data attribute, which is what /journal/<id> exists \
         to avoid"
    );
    assert!(
        code.contains("status.dataset.feed"),
        "journal.js no longer reads the feed id"
    );
    assert!(
        markup.contains("data-feed="),
        "the journal template does not carry the feed id, so the socket has no address"
    );

    // Every class the script paints has a rule. The item classes carry `flags`, so a missing one is
    // a trap that looks like an ordinary item.
    for class in [
        "when",
        "verb",
        "who",
        "where",
        "item",
        "progression",
        "useful",
        "trap",
        "gap-note",
        "daybreak",
        "day",
        // A name the sending client supplied. Losing its rule makes it identical to the verified
        // name beside it, which is the one thing that must never be true of it.
        "claimed",
    ] {
        assert!(
            code.contains(class),
            "journal.js no longer paints `{class}`; this lint is checking a class nobody uses"
        );
        assert!(
            styles(&css, class),
            "puna.css has no rule for `.{class}`, so the feed renders it as plain text"
        );
    }

    // --- A LINE ARRIVING, whose three halves each fail without a symptom -------------------------
    // The row opens and its text fades in over 100ms, and every piece of that is silent when it
    // breaks: the class renders as an ordinary row, a renamed keyframe is an animation the browser
    // simply does not run, and losing the follow leaves the newest line sitting BELOW the fold,
    // which reads as the feed having stopped rather than as an animation being wrong.
    assert!(
        code.contains("\" arriving\""),
        "journal.js no longer marks a live row, so nothing on the page animates"
    );
    assert!(
        code.contains(r#"frame.kind === "append""#),
        "journal.js no longer distinguishes the live frame from the replay, so either every \
         reconnect opens a hundred rows at once or nothing opens at all"
    );
    // **Anchored inside `append`, not on the name.** The first version of this asserted the file
    // mentioned `followBottom()` anywhere, which its own `function followBottom()` satisfies: the
    // call was deleted and the lint passed. Same trap as `scheduleReconnect` above, found the same
    // way.
    let appending = code
        .split_once("function append(")
        .map(|(_, rest)| rest.split_once("\n  }").map_or(rest, |(body, _)| body))
        .expect("journal.js no longer has an append");
    assert!(
        appending.contains("followBottom()"),
        "append no longer holds the bottom while a row is opening, so the line that just arrived \
         lands below the fold and stays there until the next one pushes it up"
    );
    assert!(
        !appending.contains("log.scrollTop ="),
        "append pins the bottom without recording where it put it, so the next frame reads the \
         page's own pin as a reader who scrolled there"
    );

    // **The follow gives up on the reader's own scroll and on nothing else, and that needs BOTH
    // signals.** Each has been wrong here in a different direction: distance alone was the shipped
    // bug, where a release's opening rows grew the content and the follow read its own animation as
    // a reader leaving; position alone misreads the browser's clamp, which moves the view by itself
    // whenever a row starts at no height. Either simplification looks tidier and reintroduces one of
    // them, and the failure is a feed that stops following exactly when the room is busiest.
    let moved = code
        .split_once("function readerMoved(")
        .map(|(_, rest)| rest.split_once("\n  }").map_or(rest, |(body, _)| body))
        .expect("journal.js no longer has a readerMoved");
    assert!(
        moved.contains("pinnedAt") && moved.contains("nearBottom()"),
        "readerMoved decides on one signal; it takes the position this page last wrote AND the \
         distance from the end, or it mistakes either the browser's clamp or its own animation for \
         somebody scrolling"
    );
    let follow = code
        .split_once("function followBottom(")
        .map(|(_, rest)| rest.split_once("\n  }").map_or(rest, |(body, _)| body))
        .expect("journal.js no longer has a followBottom");
    assert!(
        follow.contains("readerMoved()") && !follow.contains("nearBottom()"),
        "the follow decides for itself whether the reader moved, rather than asking the one \
         function that knows how to tell that from the page's own rows growing"
    );

    // A burst is not an event: past a handful of records arriving together nothing is individually
    // perceivable, and the animation becomes a few hundred simultaneous height changes on the
    // busiest thing a room ever does.
    assert!(
        code.contains("ARRIVE_MAX"),
        "journal.js no longer caps how many rows may open at once, so a release animates every line \
         of itself"
    );

    let plain = code_only_css(&css);
    for rule in [
        ".journal .entry.arriving {",
        ".journal .entry.arriving > * {",
    ] {
        let body = plain
            .split_once(rule)
            .unwrap_or_else(|| {
                panic!("puna.css has no `{rule}` rule, so a live row renders as an ordinary one")
            })
            .1;
        let body = body.split_once('}').expect("an unclosed rule").0;
        let name = body
            .split_once("animation:")
            .unwrap_or_else(|| panic!("`{rule}` no longer animates anything"))
            .1
            .split_whitespace()
            .next()
            .expect("an animation name");
        assert!(
            plain.contains(&format!("@keyframes {name}")),
            "`{rule}` names `{name}` and puna.css defines no such keyframes, so the browser runs \
             nothing at all and the rule looks correct"
        );
    }

    // Vestibular disorders are a real reason somebody turns animation off, and a feed is a page
    // people leave open for hours. Held here rather than trusted, because the rule that honors it
    // is three sections away from the rules it turns off.
    assert!(
        reduced_motion_blocks(&plain).any(|block| block.contains(".entry.arriving")),
        "no `prefers-reduced-motion` rule turns the arriving animation off"
    );

    // The scheme is derived, never written: hardcoding either breaks in exactly one environment,
    // and it is the one nobody develops in.
    assert!(
        code.contains("location.protocol") && code.contains("wss:") && code.contains("ws:"),
        "journal.js no longer derives its scheme from the page's"
    );

    // --- THE CONNECTION DOT, WHOSE EVERY HALF FAILS WITHOUT A SYMPTOM ----------------------------
    // The script sets `link-state up` / `link-state down`; the stylesheet colors them. Break either
    // side and the page still works perfectly (the feed streams, the message says the right thing)
    // and the indicator is simply always the same color. Nothing throws and nothing logs.
    for class in ["link-state", "up", "down"] {
        assert!(
            code.contains(class),
            "journal.js no longer sets `{class}` on the connection dot"
        );
    }
    for rule in [".link-state", ".link-state.up", ".link-state.down"] {
        assert!(
            css.contains(rule),
            "puna.css has no `{rule}` rule, so the connection dot cannot show that state"
        );
    }

    // --- RECONNECTION IS GATED ON VISIBILITY -----------------------------------------------------
    // The Page Visibility API, and both halves are needed: `visibilityState` to decide, and the
    // `visibilitychange` listener to notice. With the listener gone a tab that dropped while hidden
    // stays disconnected FOREVER: the redial is never scheduled and nothing else would ever
    // schedule it, so the page sits on a red dot until somebody reloads it.
    assert!(
        code.contains("document.visibilityState"),
        "journal.js no longer asks whether the tab is showing"
    );
    assert!(
        code.contains("\"visibilitychange\""),
        "journal.js no longer listens for the tab coming back, so a hidden tab that dropped never \
         reconnects at all"
    );
    // **The gate has to be inside the scheduler, and asserting the API is merely PRESENT does not
    // say that.** The first version of this lint checked only that `visibilityState` appeared
    // somewhere in the file, which it still does in the listener, so deleting the early return
    // from `scheduleReconnect` passed it. Caught by mutating exactly that.
    // Anchored on the name and the open paren, not on a full signature: it grew a `reason`
    // parameter and this assertion failed on the argument list rather than on anything it checks.
    // Loud, so cheap to fix; but a lint that breaks on an unrelated edit is a lint people delete.
    let scheduler = code
        .split_once("function scheduleReconnect(")
        .map(|(_, rest)| {
            rest.split_once("\n  }")
                .map(|(body, _)| body)
                .unwrap_or(rest)
        })
        .expect("journal.js no longer has a scheduleReconnect");
    assert!(
        scheduler.contains("showing()"),
        "scheduleReconnect no longer checks whether the tab is showing, so a background tab redials"
    );
    assert!(
        scheduler.contains("attached()"),
        "scheduleReconnect no longer checks for an existing socket, so two can be opened at once"
    );

    // --- THE DEAD-LINK WATCHDOG ------------------------------------------------------------------
    // A dropped-not-reset link leaves the socket `OPEN` for as long as TCP keeps retransmitting,
    // which is minutes, and the protocol's own ping is invisible to JavaScript, so the page has nothing to
    // go on but an ordinary frame arriving. Every piece of this is silent when removed: the page
    // keeps working perfectly and simply never notices a black hole again, which is the state it
    // was in when a five-minute outage left the dot green.
    assert!(
        code.contains("heartbeat_ms"),
        "journal.js no longer takes the heartbeat cadence from the server, so its watchdog is \
         either guessing or disarmed"
    );
    let handler = code
        .split_once("addEventListener(\"message\"")
        .map(|(_, rest)| rest)
        .expect("journal.js no longer handles messages");
    let heard = handler
        .find("heard()")
        .expect("the message handler no longer restarts the watchdog");
    let parse = handler
        .find("JSON.parse")
        .expect("the message handler no longer parses");
    assert!(
        heard < parse,
        "the watchdog is restarted only for frames that parsed, so a frame this build cannot read \
         is indistinguishable from silence"
    );

    // --- THE DOT AND THE SENTENCE CANNOT DISAGREE --------------------------------------------------
    // Reported from the browser: the watchdog announced "Lost contact…" beside a GREEN dot, because
    // it said its piece with a bare `say` and left the painting to a `close` event that had not
    // arrived yet. An indicator contradicting the words it exists to reinforce is worse than having
    // neither, so `setLink` and the `live` flag are reachable ONLY through `linkUp`/`linkDown`.
    //
    // Counted rather than merely present: the point is that there are no OTHER call sites.
    assert_eq!(
        code.matches("setLink(").count(),
        3,
        "setLink is called somewhere other than its definition and the two link* helpers, so the \
         dot can be painted without the sentence agreeing"
    );
    assert_eq!(
        code.matches("live = ").count(),
        3,
        "the live flag is assigned outside linkUp/linkDown, so it can drift from the dot"
    );
    // --- A DISOWNED SOCKET STAYS DISOWNED ----------------------------------------------------------
    // The watchdog abandons a socket rather than waiting on `close`, because `close()` starts a
    // handshake that a black-holed link never completes. That leaves a socket whose events are still
    // coming, and every one of its four handlers has to ignore them, the `error` arm most of all,
    // which used to close whatever was current rather than its own and would have torn down the
    // healthy replacement.
    assert_eq!(
        code.matches("if (mine !== epoch) return;").count(),
        4,
        "not every socket handler is guarded by its epoch, so a disowned socket can still act on \
         the page after the watchdog gave up on it"
    );
    // Line-anchored, not a bare `contains`. The first spelling of this forbade the substring
    // `socket.addEventListener(`, which the CORRECT form `sock.addEventListener(` also contains,
    // so the assertion failed on the fix. A negative assertion has to forbid the shape rather than
    // a spelling that something legitimate ends with.
    assert!(
        !code
            .lines()
            .any(|l| l.trim_start().starts_with("socket.addEventListener(")),
        "a handler is bound to the shared `socket` rather than to the one it belongs to, so a late \
         event from a disowned socket acts on whatever is connected now"
    );

    for helper in ["function linkUp(", "function linkDown("] {
        assert!(
            code.contains(helper),
            "journal.js no longer has `{helper}`, which is what makes the two inseparable"
        );
    }
    // A disconnection announced without painting is exactly the shipped bug, and this is anchored
    // on the CLASS rather than on the words, deliberately.
    //
    // The first version forbade `say("Reconnecting` and `say("Lost contact`, which keyed the whole
    // guard to the opening words of two sentences. Reword either and the assertion keeps passing
    // while guarding nothing: it decays into vacuity through ordinary copy editing, which is the
    // worst way for a lint to fail because nothing ever reports it.
    //
    // `"warning"` is the down state's own signature and is not editorial: every disconnection
    // message is one, no other message is, and `linkDown` is the only thing entitled to raise it.
    assert_eq!(
        code.matches(r#""warning""#).count(),
        1,
        "the warning class is applied somewhere other than linkDown, so a disconnection can be \
         announced without the dot agreeing. That is the bug that put a green circle beside \
         \"Lost contact\". Reword the sentences freely; route them through linkDown."
    );
    // The backoff, and the one thing that must reset it. Coming back to a tab is information about
    // the reader rather than about the server, so the wait that had built up does not apply.
    assert!(
        code.contains("RETRY_MIN") && code.contains("RETRY_MAX"),
        "journal.js no longer bounds its reconnect backoff"
    );
}

/// **The feed lets go of its socket when the server is shutting down.**
///
/// Without this arm an open feed holds its pod for the whole shutdown grace: Rocket keeps doing
/// ordinary I/O until the period expires and hyper waits for open connections, while a feed socket
/// by design never completes. So the grace that exists for downloads would be paid on every rollout
/// by every reader, and the symptom is only ever "rollouts got slower".
///
/// A source lint because there is nothing to observe: deleting the arm leaves every test green, the
/// feed working, and the page reconnecting exactly as it does now, just later, after the socket is
/// cut rather than closed.
#[test]
fn the_journal_feed_closes_itself_when_the_server_is_shutting_down() {
    let source = std::fs::read_to_string(source("src/routes/journal.rs")).expect("journal.rs");

    assert!(
        source.contains("shutdown: rocket::Shutdown"),
        "the feed route no longer takes Rocket's shutdown signal"
    );
    assert!(
        source.contains("_ = &mut shutdown =>"),
        "the feed's select! no longer has a shutdown arm, so a rollout waits out the grace on it"
    );
    // Ordering matters and is invisible: on a busy room the poll arm is ready every tick, so an
    // unbiased select! would keep choosing it and spend the grace period sending records down a
    // connection that is about to close.
    let biased = source
        .find("biased;")
        .expect("the feed's select! is biased");
    let arm = source
        .find("_ = &mut shutdown =>")
        .expect("the shutdown arm");
    let poll = source.find("_ = poll.tick() =>").expect("the poll arm");
    assert!(
        biased < arm && arm < poll,
        "the shutdown arm must come first in a biased select!, before the arm that is ready every \
         tick on a busy room"
    );
    // 1001, not 1000. "Going away" tells a client to come back; "normal closure" reads as the
    // server being finished with it.
    assert!(
        source.contains("CloseCode::Away"),
        "the feed closes with a code that does not invite a reconnect"
    );
}

/// **The feed sends a heartbeat the PAGE can see, not only a protocol ping it cannot.**
///
/// The browser WebSocket API exposes no ping or pong to JavaScript: the browser answers the server
/// by itself and tells the page nothing. So on a quiet room a protocol ping proves liveness to
/// every layer except the one that has to draw the indicator, which is how a five-minute blackhole
/// left the dot green.
///
/// Both frames are asserted, because they do different jobs and dropping either is invisible:
/// without the Ping, Envoy reaps a quiet stream; without the text frame, nothing reaches the page.
#[test]
fn the_journal_feed_sends_a_heartbeat_the_page_can_observe() {
    let source = std::fs::read_to_string(source("src/routes/journal.rs")).expect("journal.rs");

    assert!(
        source.contains(r#"{"kind":"heartbeat"}"#),
        "the feed no longer sends an application heartbeat, so a page cannot tell a quiet room from \
         a dead link"
    );
    assert!(
        source.contains("ws::Message::Ping"),
        "the feed no longer pings, so Envoy closes a quiet stream out from under it"
    );
    // The cadence goes out on the opening frame so the client's watchdog derives from it. Two
    // constants in two files would drift, and the drift is dangerous in one direction: a watchdog
    // shorter than the heartbeat tears down healthy connections on a timer.
    assert!(
        source.contains("heartbeat_ms"),
        "the opening frame no longer advertises the heartbeat cadence"
    );
    assert!(
        source.contains("PING.as_millis()"),
        "the advertised cadence is a literal rather than the interval actually used"
    );
}

/// **The lobby import gates on the lobby room's author, and `import` is the only place it can.**
///
/// The rule is a pure function with its own truth table, but a rule nothing calls guards nothing,
/// and this one cannot be reached in a test, because everything around it needs a live lobby
/// answering an HTTP request. Deleting the call compiles, passes every test, and turns the import
/// back into a way to read a stranger's lobby room: paste any room id, and its players' names and
/// Discord accounts arrive in your roster.
///
/// So the call site is pinned, along with the two things that make it correct: the check happens
/// **before** any owner is written, and the admin bypass is passed in rather than decided here.
#[test]
fn the_lobby_import_refuses_a_room_whose_author_has_no_standing() {
    let code = code_only(&std::fs::read_to_string(source("src/lobby.rs")).expect("lobby.rs"));

    let body = code
        .split("pub async fn import(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the import function");

    assert!(
        body.contains("may_import("),
        "`import` no longer gates on the lobby room's author, so any room id can be bound to this \
         room and its player list read into the roster: {body}"
    );
    assert!(
        body.contains("AuthorIsNotAnOrganizer"),
        "the gate no longer refuses: {body}"
    );

    // Before anything is claimed. A check after the writes would leave the owners in place and
    // report a refusal, which is the worst of both.
    let gate = body.find("may_import(").expect("the gate");
    let write = body.find("claim_for_owners").expect("the write");
    assert!(
        gate < write,
        "the author check runs after slots have already been claimed"
    );
}

/// **A page that something redirects to with a `Flash` has to read one.**
///
/// This failed silently for the entire life of `routes/rooms.rs`. Every `Flash` in that module
/// lands on `/room/<id>` or `/room/<id>/options`, and neither route took a `FlashMessage`, so
/// "Saved. These took effect immediately", the rename confirmation, the password-rotation result
/// and every lobby-import outcome were written into a cookie and dropped by the page they were
/// addressed to.
///
/// **Nothing anywhere reports it.** The POST succeeds, the redirect is followed, the page renders
/// correctly, and the only symptom is a sentence that never appears, which reads as the feature
/// having nothing to say. It surfaced only when an import failed against a misconfigured token and
/// the page's *other* message, the persistent one, explained the result wrongly.
///
/// So: for every `Flash::…(Redirect::to("/some/path"))`, the `#[get]` route matching that path must
/// take a `FlashMessage`. Matching is by route shape rather than by rendered URL, because the
/// targets are `format!` strings and the routes are patterns.
#[test]
fn every_page_a_flash_redirects_to_reads_one() {
    let mut producers: Vec<(String, String)> = Vec::new();
    let mut readers: Vec<String> = Vec::new();

    for path in std::fs::read_dir(source("src/routes")).expect("routes/") {
        let path = path.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let code = code_only(&std::fs::read_to_string(&path).expect("a route module"));

        // Where a flash is sent. Only literal `Redirect::to(format!("…"))` targets are checked; a
        // `back` binding is resolved by finding what it was bound to in the same file.
        for cap in code.split("Redirect::to(format!(\"").skip(1) {
            if let Some(target) = cap.split('"').next() {
                producers.push((file.clone(), target.to_string()));
            }
        }

        // Where a flash is read: a GET route whose handler takes a FlashMessage.
        let mut rest = code.as_str();
        while let Some(at) = rest.find("#[get(\"") {
            rest = &rest[at + 7..];
            let Some(route) = rest.split('"').next() else {
                break;
            };
            // The handler's parameter list is the next `(` … `)` after the attribute.
            let body: String = rest.chars().take(600).collect();
            if body.contains("FlashMessage") {
                readers.push(route.to_string());
            }
        }
    }

    assert!(
        producers.len() >= 5 && readers.len() >= 5,
        "the scan found nothing to check: {} producers, {} readers",
        producers.len(),
        readers.len()
    );

    // A target matches a route when they have the same shape: same segment count, and every
    // non-parameter segment equal. `/room/{id}/options` matches `/room/<id>/options`.
    let matches = |target: &str, route: &str| {
        let t: Vec<&str> = target.trim_matches('/').split('/').collect();
        let r: Vec<&str> = route
            .split('?')
            .next()
            .unwrap_or(route)
            .trim_matches('/')
            .split('/')
            .collect();
        t.len() == r.len()
            && t.iter()
                .zip(&r)
                .all(|(t, r)| r.starts_with('<') || t.starts_with('{') || t == r)
    };

    let mut orphans = Vec::new();
    for (file, target) in &producers {
        if !readers.iter().any(|route| matches(target, route)) {
            orphans.push(format!("{file}: Flash -> {target}"));
        }
    }

    assert!(
        orphans.is_empty(),
        "these redirect with a Flash to a page that never reads one, so the message is written to \
         a cookie and silently dropped:\n  {}",
        orphans.join("\n  ")
    );
}

/// **The slot filter page's heading names the slot AND the player, and the route is what builds
/// it.**
///
/// M31 added it after a report: somebody opened this page, read the whole thing, and still did not
/// know which slot they were editing. The room name is the bold thing on the page and the eye stops
/// there, so the scope has to be inside the `<h1>` and has to say who.
///
/// **The render test beside it cannot catch this.** It constructs `FilterTemplate` with a fixture
/// `scope`, so it asserts the template renders whatever string it is handed: true and worth having,
/// and silent about whether the route builds a useful one. Dropping the player name from the
/// `format!` passes every test in the crate.
///
/// So the format string itself is pinned. It is also the `<title>`, which is why it carries no
/// trailing punctuation: the template puts the room name after it.
#[test]
fn the_slot_filter_heading_names_the_slot_and_the_player() {
    let source = std::fs::read_to_string(source("src/routes/filters.rs")).expect("filters.rs");
    let code = code_only(&source);

    let scope = code
        .split("scope: format!(")
        .nth(1)
        .and_then(|rest| rest.split(&['\n'][..]).next())
        .expect("the slot page's scope");

    assert!(
        scope.contains("{n}"),
        "the slot filter heading no longer names the slot: {scope}"
    );
    assert!(
        scope.contains("player_name"),
        "the slot filter heading no longer names the player, which is the report that put it \
         there: {scope}"
    );
}

/// **A patch carries the port the room leads with, and the route is the only place that decides.**
///
/// `port::reserved_pair` returns the pair's BASE port and the filtered listener is `base + 1`, so
/// the whole difference between a correct patch and a wrong one is one addition in one expression.
/// Removing it looks like a simplification: `base_port` is right there, already named, and the
/// route still compiles, still serves, and still round-trips its own address.
///
/// **Nothing else would notice.** `embed_server`'s tests assert the address it writes is the address
/// it was given; the room page reads its own value; and the symptom is a player on a 500-slot room
/// whose game client drowns in the full feed, which is the exact failure the `Filtered` setting
/// exists to prevent, arriving through the file that setting is supposed to configure.
///
/// So the rule is pinned where it lives: the port handed to `embed_server` is derived, never the
/// bare reservation.
#[test]
fn a_patch_embeds_the_port_the_room_leads_with() {
    let source = std::fs::read_to_string(source("src/routes/downloads.rs")).expect("downloads.rs");
    let code = code_only(&source);

    assert!(
        code.contains("leads_with_filtered()"),
        "the patch route no longer asks which port the room leads with, so a filtered room's \
         patches carry the full port: the one that drowns the clients the setting exists for"
    );
    assert!(
        code.contains("base_port + 1"),
        "nothing in the patch route reaches the filtered half of the pair"
    );

    // The call itself, which is where the two could still part company: deriving the port and then
    // passing the base one compiles and reads fine.
    let call = code
        .split("embed_server(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("the embed_server call");
    assert!(
        !call.contains("base_port"),
        "`embed_server` is handed the reservation's base port directly, so the room's choice is \
         computed and then thrown away: {call}"
    );
}

/// **The plain-text summary is served only where the tracker is open to the world.**
///
/// `/tracker/<id>/summary.txt` exists to be fetched by a chat bot holding no credential at all, so
/// it takes **no session** and decides purely on the room: `tracker_policy == Link` or `404`. Three
/// things follow from that, and each is one edit away from being lost:
///
/// - **A `members` room's progress must not leave through it.** That policy exists to say the
///   multiworld's state is not public, and this is the one route here that answers without knowing
///   who is asking.
/// - **`404`, never `403`.** A refusal that distinguishes a restricted tracker from an id that
///   names nothing is itself an answer about which unguessable ids are real, the rule `access`
///   already states and the reason it 404s a disabled tracker.
/// - **No `Session`, which is what makes the response publicly cacheable.** Take one and the answer
///   can vary by viewer, and the `public` `Cache-Control` beside it silently becomes a way to hand
///   one viewer's document to another.
///
/// The realistic regression is somebody folding this onto `access()`, which looks like removing a
/// duplicate, is how every other view here resolves, and quietly widens this one to `members`.
/// Nothing else would notice: the route keeps working, and it works for more people.
#[test]
fn the_text_summary_is_served_only_to_a_world_open_tracker() {
    let source = std::fs::read_to_string(source("src/routes/tracker.rs")).expect("tracker.rs");
    let code = code_only(&source);

    let body = code
        .split("async fn summary_text(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the summary_text handler");

    assert!(
        body.contains("TrackerPolicy::Link"),
        "summary_text no longer requires a world-open tracker, so a `members` room's progress \
         leaves through a route that asks nobody who they are: {body}"
    );
    assert!(
        body.contains("not_found"),
        "summary_text refuses with something other than a 404, which tells a prober that a \
         restricted tracker id is real: {body}"
    );
    assert!(
        !body.contains("access("),
        "summary_text resolves through `access`, which admits a `members` room to anyone the room \
         knows, since this route answers without a session, so it must gate on the ROOM alone: {body}"
    );

    // The signature, not the body: a `Session` here would make the answer viewer-dependent, and the
    // `public` cache directive below it is only sound because it cannot be.
    let signature = body.split(')').next().unwrap_or_default();
    assert!(
        !signature.contains("Session"),
        "summary_text takes a session, so its answer can vary by viewer, while still being served \
         `public` to any shared cache in front of it: {signature}"
    );
    assert!(
        body.contains("public, max-age="),
        "summary_text no longer marks its answer publicly cacheable, which is the whole benefit of \
         it being identical for every reader: {body}"
    );
}

/// **The feed's height comes from the page, and both halves of that are silent when broken.**
///
/// `.journal` deliberately carries no height of its own: `.feed-page` makes the body a column and
/// hands the feed whatever the window has left, so the frame ends where the window does and the
/// document around it never scrolls. Two files have to agree for that, and neither failure looks
/// like a failure:
///
/// - **The template loses the class** and `.journal` has no height at all, so it grows with its
///   records. On a room with a long history that is a page which is nothing but scrollbar, the
///   exact state this replaced, arrived at from the other direction.
/// - **The stylesheet loses the rule** and the class is inert markup.
///
/// Neither errors, neither logs, and both render a page that looks broadly right until somebody
/// scrolls. So the two are pinned against each other rather than left to be noticed.
#[test]
fn the_feed_page_is_sized_to_the_window() {
    let markup = std::fs::read_to_string(source("templates/rooms/journal.html")).expect("template");
    let css = code_only_css(&std::fs::read_to_string(source("static/css/puna.css")).expect("css"));

    // **Anchored on the block itself, not on the file.** The first version of this asked whether
    // `feed-page` appeared anywhere in the template, and the comment above the block names the
    // class in order to explain it, so the lint passed with the class deleted. Fourth time a lint
    // in this file has matched its own prose, and the second caught by mutating it first.
    let layout = markup
        .split("{% block layout %}")
        .nth(1)
        .and_then(|rest| rest.split("{% endblock %}").next())
        .expect("a `layout` block");
    assert!(
        layout.contains("feed-page"),
        "the journal template no longer opts into `feed-page`, so `.journal` has no height and the \
         feed grows with its records until the page is nothing but scrollbar: {layout:?}"
    );

    for rule in [".feed-page {", ".feed-page main {", ".feed-page .journal {"] {
        assert!(
            css.contains(rule),
            "puna.css has no `{rule}` rule, so the feed page's class is inert markup"
        );
    }

    // **The floor lives on `.journal` and nowhere else.** A fixed height here is what caused the
    // original defect: 70vh is a guess about how tall the heading, the download line, the status
    // line and the backfill control happen to be, and the guess was wrong by a scrollbar.
    let journal = css
        .split(".journal {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("a `.journal` rule");
    assert!(
        !journal.contains("height: min(") && !journal.contains("\n  height:"),
        "`.journal` sets its own height again. The page supplies it: a fixed value here is a \
         guess about what sits above the feed, and everything above it can change: {journal}"
    );

    // Without this a flex item refuses to shrink past its content, so the feed's floor pushes
    // `main` off the bottom and the outer scrollbar comes straight back.
    let main = css
        .split(".feed-page main {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("a `.feed-page main` rule");
    assert!(
        main.contains("min-height: 0"),
        "`.feed-page main` no longer allows itself to shrink, so the page scrolls again: {main}"
    );

    let page = css
        .split(".feed-page {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("a `.feed-page` rule");

    // **`height`, not `min-height`, and this assertion exists because I shipped the other one.**
    // `min-height` is the sticky-footer idiom, and the two cases are opposites: a sticky footer
    // needs a short page to grow, this needs a long one to be capped. A floor with no ceiling leaves
    // the flex container with no definite size to distribute, so nothing shrinks, `.journal` grows
    // with every record, and the whole document scrolls: the exact failure this page's layout
    // exists to remove. It looks right, it passes every other check here, and it is only visible to
    // somebody scrolling a busy feed.
    assert!(
        page.contains("height: 100vh") && !page.contains("min-height: 100"),
        "`.feed-page` sizes itself with `min-height`, which states a floor and no ceiling, so the \
         feed is never capped and the page scrolls instead of the feed: {page}"
    );

    // `dvh` is the one that matters on a phone, where `100vh` is the viewport at its tallest and
    // sizing to it hides the foot of the feed behind the browser's own toolbar. The `vh` line is
    // the fallback, so both have to be here and in that order.
    let vh = page.find("height: 100vh");
    let dvh = page.find("height: 100dvh");
    assert!(
        matches!((vh, dvh), (Some(v), Some(d)) if v < d),
        "`.feed-page` must set `100vh` and then `100dvh`: the second is the real value and the \
         first is the fallback for anything that does not know it: {page}"
    );
}

/// Whether the stylesheet has a rule for this class *as a whole word*.
///
/// A plain `contains` is not enough and the difference is not pedantic: renaming `.journal .daybreak`
/// to `.journal .daybreak-unused` leaves the substring intact, so the lint kept passing over a class
/// with no rule. Found by mutation: the check has to end at a character that cannot continue an
/// identifier.
fn styles(css: &str, class: &str) -> bool {
    [format!(".journal .{class}"), format!(".item.{class}")]
        .iter()
        .any(|selector| {
            css.match_indices(selector.as_str()).any(|(at, _)| {
                css[at + selector.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'))
            })
        })
}

/// **The whole-journal download refuses a filtered viewer, in the route.**
///
/// The file carries `chat` (every line anybody typed in the room) and is therefore *where the
/// records the socket withholds actually live*. So on a room whose policy is `feed`, serving the
/// file hands over exactly what was just filtered, and the filter becomes decorative. That
/// difference lives in one `if` that no unit test reaches: removing it left every test in
/// `routes::journal` green while serving the room's chat to anyone holding the link.
///
/// Found by mutation, which is the only reason this exists. The same shape as `a_restart_would_land`:
/// a rule with a good test and an unpinned call site.
///
/// **Still one `if` after the policy became per-room**, because the policy is resolved into
/// `Visibility` by `readable` and never re-read here. A route that branched on `journal_policy`
/// itself would be a second copy of the rule, and the copies would differ the day a fourth value
/// is added.
#[test]
fn the_journal_download_is_gated_in_the_route() {
    let source = std::fs::read_to_string(source("src/routes/journal.rs")).expect("journal.rs");
    let code = code_only(&source);
    let download = code
        .split_once("async fn download(")
        .expect("a download route")
        .1;
    let body = download.split_once("\n}\n").map_or(download, |(b, _)| b);
    assert!(
        body.contains("visibility != Visibility::Everything"),
        "the journal download does not refuse a filtered viewer. The file is where the withheld \
         records live, and that check is the only thing separating it from the feed."
    );
}

/// **The feed's second gate is a refusal, and it is asked in `readable`.**
///
/// `journal_policy` decides how much of the history a non-organizer gets, and `disabled` means
/// none, but the whole of that decision is one `?` on an `Option` in a function whose other gate
/// looks very similar. Delete it and everything keeps working: the page renders, the socket
/// streams, the download behaves, and the setting an organizer chose does nothing at all, on every
/// room, silently.
///
/// Pinned in the route rather than only in `visibility_for`'s unit tests for the reason the
/// download above records: a rule can be perfectly tested and never called.
#[test]
fn a_disabled_journal_is_refused_where_the_room_is_resolved() {
    let source = std::fs::read_to_string(source("src/routes/journal.rs")).expect("journal.rs");
    let code = code_only(&source);
    let readable = code
        .split_once("async fn readable(")
        .expect("a readable()")
        .1;
    let body = readable.split_once("\n}\n").map_or(readable, |(b, _)| b);
    assert!(
        body.contains("visibility_for(role, room.journal_policy)"),
        "readable() does not consult the room's journal policy, so `disabled` reads as `feed` and \
         the setting is inert"
    );
    assert!(
        body.contains("ok_or_else"),
        "readable() reads the journal policy without refusing on it: a `disabled` room must 404, \
         never fall back to a narrower feed"
    );
}

/// **The room page offers the feed link on the same two gates the feed answers.**
///
/// They came apart the moment the policy became per-room: a public tracker over a staff-only feed
/// is an ordinary configuration, so a page keyed on `can_see_tracker` alone renders a link that
/// 404s for every viewer of every such room. The failure is quiet from both ends: the page is
/// valid, the route is correct, and only somebody who clicks finds out.
///
/// Asserted against the route rather than the markup because the markup half is covered by a render
/// test; what cannot be seen there is whether the flag it renders was ever computed from the
/// policy.
#[test]
fn the_feed_link_is_gated_on_the_policy_as_well_as_the_tracker() {
    let source = std::fs::read_to_string(source("src/routes/rooms.rs")).expect("rooms.rs");
    let code = code_only(&source);
    assert!(
        code.contains("let can_see_journal = can_see_tracker")
            && code.contains("journal::visibility_for(role, room.journal_policy)"),
        "the room page decides the feed link without asking the room's journal policy, so a \
         staff-only feed is still linked to everybody"
    );
}

/// **No template reads a credential straight off the `Room`.**
///
/// `RoomTemplate` and `PanelTemplate` both carry the whole `Room`, which has `password` and
/// `admin_token` on it, so on the room page, which is **public**, the room's shared password and
/// the bearer token that drives its admin API are both sitting in the rendering context of a page
/// an anonymous visitor is looking at.
///
/// Nothing renders them, and nothing may: the password goes out through `room_password`, a separate
/// field the *route* fills in only for participants and staff. The distinction matters because a
/// template cannot prove a negative: `{% if is_staff %}{{ room.password }}{% endif %}` looks like
/// a gate and is one, right up until the condition is edited, moved, or copied into a branch that
/// renders for somebody else. Deciding it in Rust means the value is simply absent.
///
/// The same argument `SlotView` has carried since M19, asserted here because the room page acquired
/// a second way to make the mistake the day the shared password got a reader at all.
#[test]
fn no_template_renders_a_credential_off_the_room() {
    for path in templates() {
        let source = std::fs::read_to_string(&path).expect("a template");
        let code = blank_comments(&source);
        for forbidden in ["room.password", "room.admin_token"] {
            assert!(
                !code.contains(forbidden),
                "{}: renders `{forbidden}` directly. Credentials reach a template through a field \
                 the route gated (see `room_password_for`) because a condition in markup cannot \
                 prove what it did not render.",
                label(&path)
            );
        }
    }
}

/// **`localtime.js` is the only file that decides how an instant is spelled.**
///
/// A bare `toLocaleString` renders `24/08/2026, 06.07.58` for one reader, `8/24/2026, 6:07:58 AM`
/// for another, and **no timezone for either**, so a date that is meant to say *how stale this is*
/// says something different to every reader and cannot be pasted, sorted or compared. `localtime.js`
/// settled that at M26: fixed field order, and only the ZONE localized, because the zone is the one
/// part a reader cannot infer.
///
/// The tracker's stale-document banner used `toLocaleString` anyway (reported from a live room),
/// and nothing could have caught it. It renders, it is plausible, it is *correct* in the reader's own
/// locale, and the ambiguity is invisible to whoever wrote it because their browser resolves it the
/// way they expect. That is the whole argument for a lint rather than a code review: the failure is
/// only visible to a reader in a different locale from the author, which on the tracker (the page
/// built to be shared with an audience the organizers do not choose) is most of the audience.
///
/// **Forbids the shape, not one spelling.** `toLocaleDateString` and `toLocaleTimeString` have the
/// same defect, so the anchor is `.toLocale`; matching only the exact call this bug used would walk
/// straight past the next one. That lesson is [[puna-silent-breakage]] #27's, from the `.json` lint.
///
/// **Comments are stripped first**, and that guard is precautionary rather than currently
/// load-bearing, stated precisely, because claiming otherwise would be the same class of error this
/// file exists to catch. Today's explanations write `toLocaleString` without a leading dot, so they
/// do not collide with the anchor. A comment that names the *call* (`d.as_of.toLocaleString()`, the
/// natural way to explain what this rule refuses) does collide, and fails the lint on a correct
/// file without it. Verified by adding exactly that comment and watching the unguarded form reject
/// `tracker.js`. Four lints in this project have shipped with that bug.
#[test]
fn only_localtime_js_decides_how_an_instant_is_spelled() {
    let dir = source("static");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("static/") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        // The one file allowed an opinion, being the one that holds it.
        if path.file_name().and_then(|n| n.to_str()) == Some("localtime.js") {
            continue;
        }
        let script = std::fs::read_to_string(&path).expect("a script");
        checked += 1;
        assert!(
            !code_only(&script).contains(".toLocale"),
            "{}: formats an instant with `.toLocale*` rather than `PunaTime.absolute`. A \
             locale-ordered date with no zone cannot say how stale something is. See \
             static/localtime.js, which is the one place that decides this.",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        );
    }
    // A lint that inspects nothing passes, so say how much it looked at.
    assert!(
        checked >= 6,
        "expected to scan every script in static/, saw only {checked}"
    );
}

/// **Every `popovertarget` names an `id` that exists in the same template.**
///
/// A mismatched pair is the quietest possible failure: the button renders, it is focusable, it has
/// a tooltip, and clicking it does *nothing at all*. No console error, no network request, no
/// visual change: an operator would reasonably conclude the sanction had been applied and moved
/// on. The browser gives no feedback because a `popovertarget` pointing at nothing is not an error,
/// it is just a reference to an element that is not there.
///
/// These ids are **templated** (`ban-{{ row.id }}`), so the check is a string comparison of the
/// expressions rather than of rendered output, which is what makes it a source lint. Rendering
/// would work too, but only for the rows a test happened to build.
#[test]
fn every_popover_button_points_at_a_popover_that_exists() {
    let mut offenders = Vec::new();
    let mut pairs = 0;

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        let name = label(&path);

        let ids: Vec<&str> = attribute_values(&source, "id=\"").collect();
        for target in attribute_values(&source, "popovertarget=\"") {
            pairs += 1;
            if !ids.contains(&target) {
                offenders.push(format!(
                    "{name}: popovertarget=\"{target}\" names no element in this template"
                ));
            }
        }

        // The other half: a popover nothing can open is dead markup, and on a page where the
        // overlay carries the form, it is a control the operator cannot reach at all.
        let targets: Vec<&str> = attribute_values(&source, "popovertarget=\"").collect();
        for (at, _) in source.match_indices("<div popover id=\"") {
            let id = source[at + "<div popover id=\"".len()..]
                .split('"')
                .next()
                .unwrap_or_default();
            if !targets.contains(&id) {
                offenders.push(format!(
                    "{name}: popover id=\"{id}\" has no button that opens it"
                ));
            }
        }
    }

    assert!(
        pairs >= 4,
        "only {pairs} popover buttons found: this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a popover button that names nothing renders, focuses, and silently does nothing:\n  {}",
        offenders.join("\n  ")
    );
}

/// Every value of `<attr>="..."` in the source, as borrowed slices.
fn attribute_values<'a>(source: &'a str, attribute: &'a str) -> impl Iterator<Item = &'a str> {
    source.match_indices(attribute).map(move |(at, _)| {
        source[at + attribute.len()..]
            .split('"')
            .next()
            .unwrap_or_default()
    })
}

/// **Every `<table>` sits inside a `.scroll-x` wrapper.**
///
/// The wrapper is what scrolls. It used to be the table itself (`display: block; overflow-x: auto`),
/// and that carried two bugs worth not reintroducing. `overflow-x: auto` on an element whose
/// `overflow-y` is `visible` forces the other axis to `auto` too, which is the overflow spec rather
/// than a quirk, so every table was a vertical scroll container and any content exceeding its box by
/// a fraction drew a bar down the page. And blockifying a table shrinks the table box inside it to
/// its content, so `width: 100%` sized the wrapper and left the table hugging the left edge.
///
/// Now that the scrolling lives on a wrapper, a table added without one does not degrade gracefully:
/// it overflows `main` and gives the whole page a horizontal scrollbar, which is the thing all of
/// this exists to avoid. The convention is invisible in the stylesheet, so it is asserted here.
#[test]
fn every_table_scrolls_inside_a_wrapper() {
    let mut offenders = Vec::new();
    let mut tables = 0;

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        let name = label(&path);

        for (at, _) in source.match_indices("<table") {
            tables += 1;
            // The wrapper is the element immediately before it, so look at the preceding markup
            // rather than anywhere in the file: a page with one wrapped table and one bare one
            // would otherwise pass.
            let before = source[..at].trim_end();
            // **Two wrappers qualify, and they are different jobs.** `.scroll-x` scrolls one axis
            // and pins the other shut, which is right for a table the page should grow to fit.
            // `.table-scroll` scrolls both, for the tracker's two tables whose length nobody chose,
            // and it has to be a single element, because `position: sticky` resolves against the
            // nearest scrollport and a header nested one wrapper deeper would slide away.
            let wrapped = ["<div class=\"scroll-x\">", "<div class=\"table-scroll"]
                .iter()
                .any(|w| {
                    before.rfind(w).is_some_and(|at| {
                        before[at..].ends_with('>') && !before[at..].contains("</")
                    })
                });
            if !wrapped {
                let line = line_of(&source, at).unwrap_or_default();
                offenders.push(format!(
                    "{name}:{line}: <table> is not wrapped in a scroll container, so it will \
                     overflow the page instead of scrolling itself"
                ));
            }
        }
    }

    assert!(
        tables >= 19,
        "only {tables} tables found: this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a table outside a scroll wrapper widens the whole page:\n  {}",
        offenders.join("\n  ")
    );
}

/// **Any rule that scrolls one axis states the other explicitly.**
///
/// `overflow-x: auto` does not leave `overflow-y` alone. The spec computes `visible` to `auto`
/// whenever the other axis is a scrolling value, so a rule that mentions only `overflow-x` has
/// quietly made its element a scroll container in **both** directions, and a box whose content
/// exceeds it by a fraction of a pixel then draws a scrollbar nobody asked for.
///
/// This has been written wrong twice in this file: once on `table` itself, and then again on the
/// `.scroll-x` wrapper introduced to fix it, one element outwards. Neither was visible in review;
/// both were visible on the page as a bar down the side of a table that fitted perfectly well.
#[test]
fn a_rule_that_scrolls_one_axis_names_the_other() {
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");
    let mut offenders = Vec::new();
    let mut scrollers = 0;

    // Declaration blocks, crudely: everything between `{` and the next `}`. Good enough for a
    // hand-written stylesheet with no nesting, and the comments are stripped first so prose about
    // overflow does not count as a declaration.
    for block in code_only_css(&css).split('}') {
        let Some((selector, body)) = block.split_once('{') else {
            continue;
        };
        if !body.contains("overflow-x:") {
            continue;
        }
        scrollers += 1;
        if !body.contains("overflow-y:") {
            offenders.push(format!(
                "{}: sets overflow-x without overflow-y, so it scrolls vertically too",
                selector.trim().replace('\n', " ")
            ));
        }
    }

    assert!(
        scrollers >= 1,
        "no overflow-x rules found: this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "overflow-x alone makes an element scroll in BOTH axes:\n  {}",
        offenders.join("\n  ")
    );
}

/// CSS with `/* ... */` comments removed, so prose about a property is not read as setting it.
/// Every `@media (prefers-reduced-motion: reduce)` block's body, as text.
///
/// Brace-counted rather than split on `}`, since these blocks wrap whole rules. Comments are the
/// caller's problem: pass [`code_only_css`] output, or a brace inside prose ends a block early.
fn reduced_motion_blocks(css: &str) -> impl Iterator<Item = &str> {
    const OPEN: &str = "@media (prefers-reduced-motion: reduce) {";
    css.match_indices(OPEN).map(|(at, _)| {
        let body = &css[at + OPEN.len()..];
        let mut depth = 1;
        let mut end = body.len();
        for (i, c) in body.char_indices() {
            if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
        }
        &body[..end]
    })
}

fn code_only_css(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// **A refusal's message reaches the operator now, so a 4xx must never carry a converted error.**
///
/// The [`Error`] responder deliberately sends no body at all: an `anyhow` chain from a database
/// failure can name tables, columns and connection strings. `refusal_as_json` in `routes::console`
/// makes an exception for statuses below 500, because the moderation dialog has to say *why* a
/// command was refused, and "409" on its own is not an answer somebody can act on.
///
/// **That exception is only safe because of an invariant this asserts**: every 4xx in this crate is
/// hand-built with `anyhow!(...)` (a literal, or a domain error's own `Display`), while everything
/// converted through `From` becomes a 500 and everything built from a foreign error is a 503. Add
/// one `Error::new(Status::BadRequest, db_error.into())` and a diesel chain starts rendering in a
/// dialog, with nothing failing anywhere.
///
/// Checked over the source because there is no request that can prove the absence of a call site.
#[test]
fn a_client_error_never_carries_a_converted_error_chain() {
    let mut offenders = Vec::new();
    let mut examined = 0;

    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|e| e == "rs") {
                into.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(&source("src"), &mut files);

    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let text = code_only(&text);
        let name = path
            .strip_prefix(source(""))
            .unwrap_or(&path)
            .display()
            .to_string();

        for (at, _) in text.match_indices("Error::new(Status::") {
            // The status and the source expression, bounded to this call rather than scanning on
            // into the next one.
            let call = &text[at..];
            let call = &call[..call.find(')').map_or(call.len(), |end| {
                // The source expression can itself contain parentheses, so take the line.
                call[..end].len().max(call.find('\n').unwrap_or(call.len()))
            })];

            // 5xx is allowed to carry anything: its body is never rendered.
            let client_error = [
                "BadRequest",
                "Forbidden",
                "NotFound",
                "Conflict",
                "Unauthorized",
            ]
            .iter()
            .any(|status| call.contains(&format!("Status::{status}")));
            if !client_error {
                continue;
            }
            examined += 1;

            if !call.contains("anyhow::anyhow!") && !call.contains("anyhow!") {
                let line = line_of(&text, at).unwrap_or_default();
                offenders.push(format!(
                    "{name}:{line}: a client error built from something other than `anyhow!(...)`, \
                     so its message may be a converted chain: {}",
                    call.lines().next().unwrap_or_default().trim()
                ));
            }
        }
    }

    assert!(
        examined >= 20,
        "only {examined} client errors found: this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a converted error would render its chain in the moderation dialog:\n  {}",
        offenders.join("\n  ")
    );
}

/// **A `<form>` given a block-level `display` must reset its margin**, because this stylesheet sets
/// one globally.
///
/// `form { margin: 0 0 1.25rem }` is right for the forms that are page sections. It is inert on the
/// ones marked `.inline`, because **vertical margins do not apply to an inline box**, and that is
/// exactly what makes the trap invisible: turning such a form into a flex container to line its
/// contents up *re-enables* a margin that was doing nothing a moment earlier.
///
/// It shipped on `/admin/users`. `td .actions form { display: flex; align-items: center }` was added
/// to stop form-wrapped glyphs riding their text baseline, and the restored 1.25rem then had
/// `align-items: center` center each form's **margin box**, floating every form-wrapped glyph about
/// half that above the bare buttons beside it. Rows whose controls happened to be all forms or all
/// buttons lined up perfectly, so it read as a row-height problem for two rounds of fixing.
#[test]
fn a_form_made_into_a_block_resets_the_margin_this_stylesheet_gives_every_form() {
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");
    let mut offenders = Vec::new();
    let mut examined = 0;

    for block in code_only_css(&css).split('}') {
        let Some((selector, body)) = block.split_once('{') else {
            continue;
        };
        let selector = selector.trim().replace('\n', " ");

        // Rules that target a `<form>` element by name. `.rename form`, `td .actions form`, and any
        // future one: a class selector cannot be checked this way and does not need to be, since
        // the global default is keyed on the element.
        if !selector.split(',').any(|s| s.trim().ends_with("form")) {
            continue;
        }
        // Only the ones that make it block-level. An inline form keeps the margin inert.
        if !(body.contains("display: flex")
            || body.contains("display: block")
            || body.contains("display: grid")
            || body.contains("display: inline-flex"))
        {
            continue;
        }
        examined += 1;

        if !body.contains("margin") {
            offenders.push(format!(
                "{selector}: makes a form block-level without resetting `margin`, so the global \
                 `form {{ margin: 0 0 1.25rem }}` comes back to life"
            ));
        }
    }

    assert!(
        examined >= 1,
        "no block-level form rules found: this lint is no longer looking at anything"
    );
    assert!(
        offenders.is_empty(),
        "a margin that was inert on an inline form applies again once it is block-level:\n  {}",
        offenders.join("\n  ")
    );
}

/// A column of controls says what it is. An empty `<th>` leaves the reader counting cells to work
/// out what the icons under it do, and it is invisible in review, because the table renders fine.
#[test]
fn every_column_has_a_heading() {
    let mut offenders = Vec::new();

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        for (at, _) in source.match_indices("<th></th>") {
            let line = line_of(&source, at).unwrap_or_default();
            offenders.push(format!("{}:{line}: an unnamed column", label(&path)));
        }
    }

    assert!(
        offenders.is_empty(),
        "these columns render controls under a blank heading:\n  {}",
        offenders.join("\n  ")
    );
}

/// **The tracker's summary row is filled by class, so the two files have to agree on the names.**
///
/// **A checkbox bound to a Rust `bool` has to post a word Rocket can parse.**
///
/// Rocket's `FromFormField for bool` accepts `""`, `"on"`, `"yes"` and `"true"` and **rejects
/// `"1"`**, so a checkbox written `value="1"` beside one that already says so fails the *whole*
/// submission, not just its own field. Every other control on the form is discarded with it, and
/// only when the box is ticked, so the form works until somebody uses the new option.
///
/// `server_password` is the exception and shows why the trap is inviting: its Rust field is an
/// `Option<String>`, which takes any value at all, so `value="1"` is correct there and is the
/// nearest example for anybody adding the next checkbox.
#[test]
fn a_checkbox_posts_something_its_rust_type_can_parse() {
    // Rocket 0.5, `form/from_form_field.rs`: `v.is_empty() || v == "on" || v == "yes" || v ==
    // "true"`. Transcribed rather than inferred, and the reason `"1"` is absent is that it is not
    // in that list.
    const ROCKET_TRUE: [&str; 3] = ["on", "yes", "true"];
    // Fields whose Rust type is `Option<String>` rather than `bool`, which accept anything.
    const NOT_A_BOOL: [&str; 1] = ["server_password"];

    let mut checked = 0;
    for entry in std::fs::read_dir(source("templates")).expect("templates") {
        let dir = entry.expect("entry").path();
        let files =
            std::fs::read_dir(&dir).map_or_else(|_| Vec::new(), |it| it.flatten().collect());
        for file in files {
            let body = std::fs::read_to_string(file.path()).unwrap_or_default();
            let mut rest = body.as_str();
            while let Some(at) = rest.find(r#"type="checkbox""#) {
                rest = &rest[at..];
                let tag = rest.split('>').next().unwrap_or_default();
                rest = &rest[r#"type="checkbox""#.len()..];

                let field = |key: &str| {
                    tag.find(key)
                        .map(|i| tag[i + key.len()..].split('"').next().unwrap_or_default())
                };
                let (Some(name), Some(value)) = (field(r#"name=""#), field(r#"value=""#)) else {
                    continue;
                };
                checked += 1;
                if NOT_A_BOOL.contains(&name) {
                    continue;
                }
                assert!(
                    ROCKET_TRUE.contains(&value),
                    "the {name:?} checkbox posts {value:?}, which Rocket's `bool` refuses: \
                     ticking it fails the entire form, and only then. Use one of {ROCKET_TRUE:?}, \
                     or make the field an Option<String> as {NOT_A_BOOL:?} are."
                );
            }
        }
    }

    assert!(
        checked >= 8,
        "only {checked} checkboxes found: this lint is no longer looking at anything"
    );
}

/// **Every progression a row can carry has a tint, and every tint belongs to one.**
///
/// The class name is built in the client as `prog-${tone}` from a value the server sends, so the
/// three sides (`ProgressionStatus::as_sql`, the template's radio values, and `puna.css`) agree
/// only because somebody kept them agreeing. A missing rule is silent: the chip renders in the
/// default muted grey and looks like a chip that was never meant to be colored.
///
/// The other direction matters too. A rule for a tone nothing emits is dead style that reads as
/// evidence the feature has a state it does not have.
///
/// `unknown` is deliberately absent from the stylesheet: it renders no chip at all, so a rule for it
/// could never apply.
#[test]
fn every_progression_has_a_tint_and_every_tint_a_progression() {
    let model = std::fs::read_to_string(source("../puna-core/src/model/annotation.rs"))
        .expect("annotation.rs");
    let css = std::fs::read_to_string(source("static/css/puna.css")).expect("puna.css");

    // The wire spellings, read out of `ProgressionStatus::as_sql` rather than listed here.
    let arms = model
        .split_once("impl ProgressionStatus {")
        .expect("ProgressionStatus is gone")
        .1;
    let arms = arms.split_once("pub fn label(").expect("no label fn").0;
    let tones: Vec<&str> = arms
        .match_indices("=> \"")
        .map(|(at, m)| {
            arms[at + m.len()..]
                .split('"')
                .next()
                .expect("a closing quote")
        })
        .filter(|tone| *tone != "unknown")
        .collect();

    assert_eq!(
        tones.len(),
        4,
        "expected four tinted progressions, found {tones:?}"
    );

    for tone in &tones {
        assert!(
            css.contains(&format!(".tag.prog-{tone} {{")),
            "`prog-{tone}` has no rule, so that chip renders in the default grey and looks \
             deliberate"
        );
    }

    // And nothing else claims to be one.
    let styled: Vec<String> = css
        .match_indices(".tag.prog-")
        .map(|(at, m)| {
            css[at + m.len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    for tone in &styled {
        assert!(
            tones.contains(&tone.as_str()),
            "`prog-{tone}` is styled and no progression emits it"
        );
    }
}

/// **A tab says which page before it says which room.**
///
/// A browser truncates a title from the right, so the surviving half has to be the half that
/// distinguishes one of a room's tabs from another. Titles used to read `<room> &mdash; tracker`,
/// which spent their first twenty characters on the thing every tab of that room had in common:
/// `Friday async — con…` and `Friday async — mem…` are the same string to a reader.
///
/// Two rules, and neither is expressible as "every title matches a format": some pages have no room
/// (`Your rooms`), one leads with an expression that *is* the page name (the batch page's action),
/// and one omits the room deliberately (a spent claim link).
///
/// 1. **No em dash in a title.** It was the old shape's separator, so its return is the regression.
/// 2. **Where a title names the room, the room comes after a colon.** That is the ordering, checked
///    positionally rather than by matching a whole format.
///
/// Neither can be caught by rendering: the page is correct either way, and the cost is paid by
/// somebody squinting at a strip of tabs.
#[test]
fn a_tab_names_its_page_before_the_room_it_belongs_to() {
    let mut titles = 0;
    let mut with_a_room = 0;

    for path in templates() {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
        let Some(block) = source
            .split_once("{% block title %}")
            .and_then(|(_, rest)| rest.split_once("{% endblock %}"))
            .map(|(block, _)| block)
        else {
            continue;
        };
        titles += 1;
        let name = label(&path);

        assert!(
            !block.contains("&mdash;"),
            "{name}: the title is back to the dashed shape, which puts the room first: {block}"
        );

        // **Control tags are blanked first, and that is not tidiness.** The redeem page's title is
        // `{{ page }}{% if let Some(room) = room_name %}: {{+ room }}{% endif %}`: the earliest
        // mention of `room_name` is the *binding*, which renders nothing and sits before the colon
        // that the value it binds sits after. Reading the raw text failed that page for having the
        // right order.
        let rendered = blank_tags(block);

        // Which expression is the room has to be named: a title's other expressions are page names
        // (the batch page's action) and site names, and nothing in the string says which is which.
        let Some(at) = expressions(&rendered)
            .find(|(_, expr)| matches!(*expr, "room.name" | "room_name" | "room"))
            .map(|(at, _)| at)
        else {
            continue;
        };
        with_a_room += 1;

        assert!(
            rendered[..at].contains(':'),
            "{name}: the room is named before the page is, so a truncated tab says which room and \
             not which page: {block}"
        );
    }

    assert!(
        titles >= 20 && with_a_room >= 9,
        "read {titles} titles and {with_a_room} that name a room: this lint is no longer looking \
         at anything"
    );
}

/// **The sort direction is `compare`'s to apply, and negating its answer breaks a rule it states.**
///
/// `compare` puts nulls last *in both directions*: an untouched slot belongs at the end of "least
/// recently seen" and of "most recently seen" alike, because it has no answer either way. A caller
/// that multiplies the result by `-1` for a descending sort negates that too, so the nulls lead.
///
/// That is what shipped, underneath a comment saying the opposite, on the two columns whose null
/// rule was written down on purpose: `last seen` and `checks`. It is invisible in review because
/// both halves read correctly on their own, and invisible in use because a column of nulls at the
/// wrong end still looks sorted.
///
/// Forbidding the sign flip rather than trusting the parameter: passing `dir` in and negating on
/// the way out are not exclusive, and doing both is the same bug with an extra argument.
#[test]
fn the_sort_direction_is_applied_inside_compare_rather_than_by_its_caller() {
    let script = std::fs::read_to_string(source("static/tracker.js")).expect("tracker.js");
    // Comments name the thing they warn about, so a lint reading them rejects the correct file,
    // which has happened here before and teaches the next person to delete the lint.
    let code: String = script
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("function compare(a, b, type, dir)"),
        "`compare` no longer takes the direction, so its caller must be flipping the sign"
    );
    assert!(
        code.contains("compare(valueOf(a), valueOf(b), type, dir)"),
        "the sort no longer hands `compare` the direction"
    );
    for flip in [r#"dir === "asc" ? 1 : -1"#, r#"dir === "desc" ? -1 : 1"#] {
        assert!(
            !code.contains(&format!(
                "compare(valueOf(a), valueOf(b), type, dir)) * ({flip}"
            )),
            "the sort negates `compare`, which sends the nulls to the wrong end"
        );
    }
    // The general shape, wherever it is written: nothing outside `compare` decides direction.
    let inside = code
        .split_once("function compare(a, b, type, dir)")
        .expect("compare")
        .1;
    let outside = code.replace(inside, "");
    assert!(
        !outside.contains(r#"=== "asc" ? 1 : -1"#),
        "something outside `compare` is applying the sort direction"
    );
}

/// **Every `sortValues` key is a column that exists**, or the column it meant to fix sorts by
/// nothing.
///
/// `data-key` is a header's identity and its default sort is `row[key]`; `sortValues` overrides that
/// for a column whose display and its ordering differ. The two live in different files and are
/// joined only by the string, so a rename in either one lands here:
///
/// * an entry naming no header is dead: the column it was written for went back to the default,
///   which is what "checks" sorting by raw count looked like before it was fixed;
/// * a header whose entry lost its name falls back to `row["held_by"]`, which is `undefined` on
///   every row, so every row compares equal and the table simply does not reorder.
///
/// Both draw the arrow and neither logs anything. The reverse direction (a header with no entry)
/// is deliberately **not** flagged: most columns want the default, and only a column whose key is
/// not a field on the row needs one, which is not a thing this can see from here.
#[test]
fn every_sort_override_names_a_column_that_exists() {
    let script = std::fs::read_to_string(source("static/tracker.js")).expect("tracker.js");
    let markup = std::fs::read_to_string(source("templates/tracker/show.html")).expect("show.html");

    let block = script
        .split_once("sortValues: {")
        .expect("tracker.js no longer configures any sort override")
        .1
        .split_once('}')
        .expect("unterminated sortValues")
        .0;

    // `key: ...` per line, comments and blanks skipped. The block is a literal by construction
    // (it is a config object), so this does not have to parse JavaScript to read it.
    let keys: Vec<&str> = block
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .filter_map(|line| line.split_once(':').map(|(key, _)| key.trim()))
        .collect();

    assert!(
        keys.len() >= 2,
        "read {} sort overrides out of tracker.js: this lint is no longer looking at anything",
        keys.len()
    );
    for key in keys {
        assert!(
            markup.contains(&format!(r#"data-key="{key}""#)),
            "`sortValues.{key}` overrides the sort for a column no header declares, so the column \
             it was written for is back on the default and this entry does nothing"
        );
    }
}

/// **The Owner cell is built from the server's flag, never from whether the row has an owner.**
///
/// The `<th>` is server-rendered from `{% if annotations %}` and the body is the client's, so the
/// two agree only because both read one flag. Building the cell from `r.owner` instead is the
/// obvious simplification and is wrong in a way that is invisible on a healthy room: **an unclaimed
/// slot carries no owner**, so those rows would get one fewer cell than the header declares and
/// every column after it would slide left (checks under Game, status under Checks) on some rows
/// and not others.
///
/// It reads as data rather than as a bug, which is why this forbids the shape rather than trusting
/// the comment beside it. Nothing in the Rust build parses this file, and the mutation compiles.
#[test]
fn the_owner_cell_is_gated_on_the_flag_rather_than_on_the_data() {
    let script = std::fs::read_to_string(source("static/tracker.js")).expect("tracker.js");
    let code: String = script
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("annotations ? [ownerCell(r)] : []"),
        "the owner cell is no longer built from the server-rendered flag"
    );
    // The two spellings a rewrite reaches for, both of which drop the cell on unclaimed rows.
    for wrong in ["r.owner ? [ownerCell", "r.owner && [ownerCell"] {
        assert!(
            !code.contains(wrong),
            "`{wrong}` builds the Owner cell only for claimed slots, so every column after it \
             shifts left on the rows that have no owner"
        );
    }
    // And the flag itself comes from the DOM rather than from a row.
    assert!(
        code.contains(r#"root.dataset.annotations === "1""#),
        "the client no longer reads the same flag the <th> is rendered from"
    );
}

/// **The unclaimed tag has to test for `false`, not for falsiness**, and an author cannot see the
/// difference.
///
/// `claimed` is omitted from the JSON entirely for a viewer who may not know: the room's staff and
/// slot holders get it, nobody else does. So `r.claimed ? … : { tag: "unclaimed" }`, which is what
/// this line was and what a tidy-up would restore, reads `undefined` as "not claimed" and tags
/// **every slot** `unclaimed` for exactly the anonymous audience the server just declined to tell.
///
/// Nothing catches that in practice. The server-side gate is what withholds the data and it has its
/// own test; this is about the rendering, and the rendering is only wrong when *logged out*, which
/// is the one state somebody editing the tracker is least likely to be in. A page that reads
/// correctly for its author and lies to everybody else is the exact shape this file exists for.
///
/// Anchored on the property rather than on one spelling: the field may be tested for `false`, and
/// may not be tested for truthiness alone.
#[test]
fn the_unclaimed_tag_distinguishes_withheld_from_unclaimed() {
    let script = std::fs::read_to_string(source("static/tracker.js")).expect("tracker.js");
    let code: String = script
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("r.claimed === false"),
        "the unclaimed tag no longer distinguishes a withheld claim state from an unclaimed slot"
    );
    // The shape that would restore the bug, in the two spellings a rewrite reaches for. Checked
    // against the comment-stripped copy, because the doc above names the broken form on purpose
    // and a lint that matches its own prose fails on a correct file.
    for wrong in ["r.claimed ?", "r.claimed?", "!r.claimed"] {
        assert!(
            !code.contains(wrong),
            "`{wrong}` reads a withheld claim state as unclaimed and tags every slot for logged-out \
             viewers"
        );
    }
}

/// `tracker.js` returns an object from `summary` and looks each key up as `tfoot .KEY`; the template
/// renders the cells. Rename one on either side and the lookup answers `null`, which `renderSummary`
/// steps over deliberately, so the row still appears, still spans the right columns, and one cell
/// is silently blank. Nothing errors and nothing logs, which is the same shape as the
/// `panel.dataset` and `popovertarget` lints.
///
/// It also counts the footer against the header. A `colspan` that stops matching the column count is
/// the M26 failure in a new place: a summary drifting one column left puts the check total under
/// "Status" and looks like data rather than like a bug.
#[test]
fn the_tracker_summary_fills_every_cell_it_declares() {
    let script = std::fs::read_to_string(source("static/tracker.js")).expect("tracker.js");
    let template =
        std::fs::read_to_string(source_template("tracker/show.html")).expect("tracker/show.html");

    // The keys the script will look for: the `return { ... }` inside `summary`.
    let body = script
        .split_once("summary: (rows) => {")
        .expect("tracker.js no longer declares a `summary` builder")
        .1;
    let returned = body
        .split_once("return {")
        .expect("`summary` no longer returns an object literal")
        .1
        .split_once("};")
        .expect("unterminated `summary` return")
        .0;

    let keys: Vec<&str> = returned
        .lines()
        .filter_map(|line| line.trim().split_once(':').map(|(key, _)| key.trim()))
        .filter(|key| !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()))
        .collect();

    // A source lint that matches nothing passes. This one has a known set to find.
    assert!(
        keys.len() >= 3,
        "expected the summary to declare at least three cells, found {keys:?}"
    );

    for key in &keys {
        assert!(
            template.contains(&format!("class=\"{key}\"")),
            "`tracker.js` fills `tfoot .{key}` and tracker/show.html renders no such cell, so that \
             column of the summary would be blank with nothing reporting it. Found: {keys:?}"
        );
    }

    // The footer has to span exactly the columns its OWN table declares, scoped to the slots
    // section, since this template holds four tables and a whole-file count would be meaningless.
    let slots = template
        .split_once("data-view=\"slots\"")
        .expect("the slots table is gone")
        .1
        .split_once("</section>")
        .expect("unterminated slots section")
        .0;
    let headings = slots.matches("<th data-key=").count();
    let foot = slots
        .split_once("<tfoot")
        .expect("the multiworld summary row is gone")
        .1
        .split_once("</tfoot>")
        .expect("unterminated <tfoot>")
        .0;
    // Each `colspan="2"` covers one column beyond the cell it sits on.
    let spanned = foot.matches("<td").count() + foot.matches("colspan=\"2\"").count();
    assert_eq!(
        spanned, headings,
        "the summary spans {spanned} columns and the slot table declares {headings}; a footer that \
         drifts puts the check total under the wrong heading and reads as data"
    );
}

/// **Block containers have to close as often as they open.**
///
/// Written after leaving a `<fieldset>` unclosed on the bulk panel, which nested the next one inside
/// it and gave the page two legends for one box. **Browsers repair this silently.** The markup is
/// never rejected, nothing logs, and the rendered result is merely subtly wrong: a nested fieldset
/// inherits the outer one's disabled state and border, and a screen reader announces the wrong
/// grouping. It reads as a styling problem, which is the wrong place to look.
///
/// Counting rather than parsing, because a real parser is not worth it here and an imbalance is the
/// whole failure: a template where these agree can still be malformed, but every malformation of
/// this shape shows up in the count.
#[test]
fn every_block_container_a_template_opens_is_closed() {
    // Deliberately not `<div>`: askama branches legitimately open one in an `{% if %}` and close it
    // in the matching `{% else %}` arm, so a count over the source is meaningless there. These
    // three are always written as a matched pair in this codebase.
    const TAGS: &[&str] = &["fieldset", "table", "form"];
    let mut offenders = Vec::new();

    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        for tag in TAGS {
            let opens = source.matches(&format!("<{tag}")).count();
            let closes = source.matches(&format!("</{tag}>")).count();
            if opens != closes {
                offenders.push(format!(
                    "{}: {opens} <{tag}> against {closes} </{tag}>",
                    label(&path)
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these templates open a block they do not close, which browsers repair into something \
         subtly different rather than rejecting:\n  {}",
        offenders.join("\n  ")
    );
}

/// **The bulk panel's buttons and its action table have to name the same set.**
///
/// `ACTIONS` in `routes/bulk.rs` decides what the route will do; the buttons in `rooms/bulk.html`
/// decide what an operator can ask for. Drift either way is silent in a different direction: an
/// action in the table with no button is unreachable and looks like it was never built, and a button
/// whose value is not in the table posts an action the route answers `400` to, from a control that
/// looks exactly like the six beside it that work.
///
/// Also pins the field name, because the panel's whole submit mechanism rests on it: the staged
/// `<select multiple>` **is** the form field, so a rename posts an empty slot list and every action
/// answers "nothing was staged" with no error anywhere.
#[test]
fn the_bulk_panel_offers_exactly_the_actions_its_route_implements() {
    let route = std::fs::read_to_string(source("src/routes/bulk.rs")).expect("bulk.rs");
    let template =
        std::fs::read_to_string(source_template("rooms/bulk.html")).expect("rooms/bulk.html");

    let table = route
        .split_once("const ACTIONS:")
        .expect("bulk.rs no longer declares ACTIONS")
        .1
        .split_once("];")
        .expect("unterminated ACTIONS")
        .0;

    // `("name", "Label")`: take the first string of each pair.
    let declared: Vec<&str> = table
        .lines()
        .filter_map(|line| line.trim().strip_prefix("(\""))
        .filter_map(|rest| rest.split('"').next())
        .collect();

    assert!(
        declared.len() >= 5,
        "only {} actions parsed out of ACTIONS: this lint is no longer looking at anything, \
         {declared:?}",
        declared.len()
    );

    for action in &declared {
        assert!(
            template.contains(&format!("value=\"{action}\"")),
            "`{action}` is in ACTIONS and has no button in rooms/bulk.html, so nobody can ask for \
             it. Declared: {declared:?}"
        );
    }

    // And the other direction: a button the route cannot serve.
    let mut rest = template.as_str();
    while let Some(at) = rest.find("name=\"action\" value=\"") {
        let after = &rest[at + "name=\"action\" value=\"".len()..];
        let value = after.split('"').next().unwrap_or_default();
        assert!(
            declared.contains(&value),
            "rooms/bulk.html offers `{value}` and ACTIONS does not list it, so pressing it is a 400"
        );
        rest = after;
    }

    assert!(
        template.contains("name=\"slots\""),
        "the staged list is the form field; renaming it posts nothing and every action reports \
         that nothing was staged"
    );
}

/// **Every hook `moderation.js` reaches for has to exist in the markup.**
///
/// The script addresses the dialog entirely through `[data-mod-…]` attributes, and most of those
/// reads are unguarded: `form.querySelector("[data-mod-status]").value = …` throws on `null`. So a
/// renamed or dropped attribute does not degrade one field, it throws inside the click handler and
/// **every control in the moderation column stops doing anything**, with the only evidence in a
/// console nobody has open. The same contract-across-two-files shape as the `panel.dataset` lint,
/// and the same failure mode.
///
/// Written the strict way round (the script is the authority, the template must satisfy it), since
/// an unused attribute in the markup is harmless and a missing one is not.
#[test]
fn the_moderation_dialog_renders_every_hook_its_script_reaches_for() {
    let script = std::fs::read_to_string(source("static/moderation.js")).expect("moderation.js");
    let template =
        std::fs::read_to_string(source_template("rooms/show.html")).expect("rooms/show.html");

    let mut wanted: Vec<&str> = Vec::new();
    let mut rest = script.as_str();
    while let Some(at) = rest.find("[data-mod-") {
        let after = &rest[at + 1..];
        let name = after.split(']').next().unwrap_or_default();
        if !name.is_empty() && !wanted.contains(&name) {
            wanted.push(name);
        }
        rest = after;
    }

    assert!(
        wanted.len() >= 8,
        "only {} hooks found in moderation.js: this lint is no longer looking at anything, {wanted:?}",
        wanted.len()
    );

    let missing: Vec<&&str> = wanted
        .iter()
        .filter(|name| !template.contains(**name))
        .collect();
    assert!(
        missing.is_empty(),
        "moderation.js addresses these and rooms/show.html renders none of them, so the first \
         click in the moderation column throws and every control there goes dead: {missing:?}"
    );
}

/// **The rule table's hooks and its field names, across three files that never mention each other.**
///
/// `filters.js` addresses everything through `[data-…]` attributes and renames fields with
/// `/^rules\[(\d+)\]/`; `rooms/_rule_table.html` renders both; and `routes/filters.rs` reads the
/// submission back as `rules[N].field`. Every break here is silent in its own way, which is why
/// this is a lint rather than a comment:
///
/// * A renamed `data-rule-*` hook makes `narrow()` return early, so the tag and subtype cells stop
///   following the kind, and a tag left on a row that is no longer a bounce is then submitted and
///   refused, in a form that looked fine.
/// * A renamed `data-empty-meaning` leaves the radios enabled and `required` while hidden, and the
///   browser then refuses to submit the form with a validation bubble it cannot point at anything.
/// * A field named anything but `rules[N].…` still posts, and Rocket reads **nothing**: the route
///   sees an empty table and clears the filter that was on screen a moment ago.
///
/// The template is checked against the script, and the field names against both.
#[test]
fn the_rule_table_renders_every_hook_and_field_name_its_readers_expect() {
    let script = std::fs::read_to_string(source("static/filters.js")).expect("filters.js");
    let template = std::fs::read_to_string(source_template("rooms/_rule_table.html"))
        .expect("rooms/_rule_table.html");

    let mut wanted: Vec<&str> = Vec::new();
    let mut rest = script.as_str();
    while let Some(at) = rest.find("[data-") {
        let after = &rest[at + 1..];
        let name = after.split(']').next().unwrap_or_default();
        if !name.is_empty() && !wanted.contains(&name) {
            wanted.push(name);
        }
        rest = after;
    }

    assert!(
        wanted.len() >= 8,
        "only {} hooks found in filters.js: this lint is no longer looking at anything, {wanted:?}",
        wanted.len()
    );

    // `data-rule-form` is the one the script looks for on the FORM, which the table itself does not
    // render; the two callers do, and both are checked below.
    let missing: Vec<&&str> = wanted
        .iter()
        .filter(|name| **name != "data-rule-form" && !template.contains(**name))
        .collect();
    assert!(
        missing.is_empty(),
        "filters.js addresses these and rooms/_rule_table.html renders none of them, so the editor \
         silently stops narrowing, striking out or asking what an empty table means: {missing:?}"
    );

    for host in ["rooms/filter.html", "rooms/bulk.html"] {
        let page = std::fs::read_to_string(source_template(host)).expect(host);
        assert!(
            page.contains("data-rule-form"),
            "{host} includes the rule table and does not mark its form, so filters.js binds \
             nothing on that page"
        );
        assert!(
            page.contains("filters.js"),
            "{host} includes the rule table and does not load filters.js"
        );

        // **The first submit button in a form is what Enter presses.** Every row of the rule table
        // ends in one, so whichever host form does not claim that role first has an Enter key that
        // deletes rule 1, and on the bulk panel, before this was claimed, one that rotated every
        // staged slot's password. Nothing about the page looks wrong either way.
        let form = page
            .split_once("<form")
            .unwrap_or_else(|| panic!("{host} has no form"))
            .1;
        let first_submit = form
            .split_once("type=\"submit\"")
            .unwrap_or_else(|| panic!("{host} has no submit button"))
            .1
            .split_once('>')
            .unwrap()
            .0;
        assert!(
            first_submit.contains("aria-hidden=\"true\""),
            "{host}'s first submit button is a real one, so Enter in any text field activates it \
             instead of the inert default: {first_submit}"
        );
    }

    // The field names, which are a contract with Rocket rather than with the script, and the one
    // whose failure looks like the room clearing its own filter.
    for field in ["direction", "kind", "tag", "subtype", "percent", "remove"] {
        assert!(
            template.contains(&format!("].{field}\"")),
            "rooms/_rule_table.html no longer renders a `rules[N].{field}` field, so the route \
             reads it as absent on every save"
        );
    }
    assert!(
        template.contains("name=\"rules["),
        "the table's fields are indexed under `rules[N]`; anything else parses as an empty table \
         and clears the filter being edited"
    );
    assert!(
        script.contains("^rules\\[(\\d+)\\]"),
        "filters.js renumbers added rows by this prefix; if it stops matching, two rows share an \
         index and Rocket silently merges them into one rule"
    );
    // **The direction constraint, which is a three-file contract like the rest.** `travels_text`
    // renders into `data-travels`, `filters.js` reads it to hide the impossible directions, and
    // `Rule::validate` refuses one that gets through anyway. Break the attribute and the editor
    // silently goes back to offering a `from_slot` `PrintJSON`: a rule the room answers `400` to,
    // over a page still showing it as saved.
    assert!(
        template.contains("data-travels="),
        "the kind picker no longer carries which directions it can travel, so the editor offers \
         pairings the room refuses"
    );
    assert!(
        script.contains("dataset.travels"),
        "filters.js no longer reads `data-travels`, so nothing narrows the direction picker"
    );

    assert!(
        template.contains("name=\"state\""),
        "the empty table's meaning is posted as `state`; renaming it makes every emptied table an \
         unanswerable question"
    );
}

/// **A filter box that scripting has not reached must not look usable.**
///
/// Three files have to agree and each spells the contract differently, so no grep in one finds the
/// others: a template renders `class="table-search"`, `table.js` adds `js-tables` to `<html>`, and
/// `puna.css` reveals `.table-controls` from that class. Break any one and the box still renders,
/// still takes focus, still accepts typing, and filters nothing, with no error anywhere. It is the
/// same failure `.theme` and `.copy` are gated against, and the same shape as the theme lint below.
///
/// The room page's own comment asserted the box was "simply absent" without scripting for as long
/// as it stood, which is how this went unnoticed: the claim was in the file and was never true.
#[test]
fn a_filter_box_is_hidden_until_the_script_that_drives_it_arrives() {
    let css = code_only_css(&std::fs::read_to_string(source("static/css/puna.css")).expect("css"));
    let script = std::fs::read_to_string(source("static/table.js")).expect("table.js");

    // The script's half.
    assert!(
        script.contains("classList.add(\"js-tables\")"),
        "table.js no longer marks the document, so every filter box stays hidden for everyone"
    );

    // The stylesheet's half, both directions: hidden by default, revealed by the class.
    assert!(
        css.contains(".table-controls { display: none; }"),
        "`.table-controls` is no longer hidden by default, so the box shows without its script"
    );
    assert!(
        css.contains(".js-tables .table-controls"),
        "nothing reveals `.table-controls`, so the box never appears at all"
    );

    // **Every page with a filter box must load something that reveals it.** This is the half that
    // was missing, and it cost exactly what it protects: the tracker has its own `tracker.js` and
    // does not load `table.js`, so wrapping one of its boxes in `.table-controls` hid the box AND
    // the toggle beside it, gated by a class nothing on that page ever set. The markup was right,
    // the stylesheet was right, and the control was invisible.
    for path in templates() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));

        // **Fragments are checked through the pages that include them, not on their own.**
        // `admin/resting.html` carries a filter box and loads nothing, because it is injected into
        // `/admin/rooms` and included by `resting_page.html`, both of which load the script. So a
        // page is expanded with whatever it includes before being asked, and a template that
        // extends nothing is skipped as a fragment.
        if !raw.contains("{% extends") {
            continue;
        }
        let page = expand_includes(&raw);
        if !page.contains("class=\"table-search\"") {
            continue;
        }
        let raw = page;

        // Which scripts this page pulls in, and whether any of them says the class.
        let reveals = raw
            .match_indices("/static/")
            .filter_map(|(at, _)| raw[at + "/static/".len()..].split(['?', '"']).next())
            .filter(|f| f.ends_with(".js"))
            .any(|file| {
                std::fs::read_to_string(source(&format!("static/{file}")))
                    .is_ok_and(|js| js.contains(r#"classList.add("js-tables")"#))
            });

        assert!(
            reveals,
            "{}: renders a filter box but loads no script that adds `js-tables`, so the stylesheet \
             keeps it hidden and the control never appears",
            label(&path)
        );
    }

    // And every box names a table that exists in the same template, the way a `popovertarget` must.
    // A typo here is a box that renders, focuses, and filters nothing.
    let mut boxes = 0;
    for path in templates() {
        let source = blank_comments(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {path:?}: {e}")),
        );
        let ids: Vec<&str> = attribute_values(&source, "id=\"").collect();
        for target in attribute_values(&source, "data-filters=\"") {
            boxes += 1;
            assert!(
                ids.contains(&target),
                "{}: data-filters=\"{target}\" names no table in this template",
                label(&path)
            );
        }
    }

    assert!(
        boxes >= 3,
        "only {boxes} filter boxes found: this lint is no longer looking at anything"
    );
}

/// **Every page with a copy control loads the script that reveals it.**
///
/// The sibling of the rule above, and the same three-way contract spelled three ways: a template
/// renders `class="copy"`, `copy.js` puts `js-copy` on `<html>` once it has proved the clipboard is
/// reachable, and `puna.css` reveals `.copy` from that class. The gate is deliberate: on plain HTTP
/// `navigator.clipboard` is absent, and a button that silently does nothing is worse than no button,
/// because the value looks copied and the paste is whatever was there before.
///
/// The failure this catches is the *other* way round: markup and stylesheet both correct, and the
/// page loading no script that ever sets the class, so the control is hidden from everybody
/// forever. The members page was exactly that the moment it grew an invite copy button: it had no
/// `{% block scripts %}` at all.
#[test]
fn a_copy_control_is_hidden_until_the_script_that_drives_it_arrives() {
    let css = code_only_css(&std::fs::read_to_string(source("static/css/puna.css")).expect("css"));
    let script = std::fs::read_to_string(source("static/copy.js")).expect("copy.js");

    assert!(
        script.contains(r#"classList.add("js-copy")"#),
        "copy.js no longer marks the document, so every copy control stays hidden for everyone"
    );
    assert!(
        css.contains(".js-copy .copy"),
        "nothing reveals `.copy`, so the controls never appear at all"
    );

    let mut pages = 0;
    for path in templates() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));

        // Fragments are checked through the pages that include them: `rooms/panel.html` carries
        // copy controls and loads nothing, because `rooms/show.html` includes it and loads the file.
        if !raw.contains("{% extends") {
            continue;
        }
        let page = expand_includes(&raw);
        if !page.contains(r#"class="copy""#) {
            continue;
        }
        pages += 1;

        let reveals = page
            .match_indices("/static/")
            .filter_map(|(at, _)| page[at + "/static/".len()..].split(['?', '"']).next())
            .filter(|f| f.ends_with(".js"))
            .any(|file| {
                std::fs::read_to_string(source(&format!("static/{file}")))
                    .is_ok_and(|js| js.contains(r#"classList.add("js-copy")"#))
            });

        assert!(
            reveals,
            "{}: renders a copy control but loads no script that adds `js-copy`, so the stylesheet \
             keeps it hidden and the button never appears",
            label(&path)
        );
    }

    // The tracker builds its own copy controls in `tracker.js` rather than in markup, so it is not
    // one of these: it loads `copy.js` for that reason and says so in its own comment.
    assert!(
        pages >= 3,
        "only {pages} pages render a copy control: this lint is no longer looking at anything"
    );
}

/// **Every shorthand duration carries the instant behind it**, and the three files that make that
/// work have to agree.
///
/// A cell reading "6d 2h" answers how long ago and cannot answer *when*, which is the question
/// somebody has once they are correlating a row with a log line or somebody else's account. The
/// exact moment goes in a `title`, rendered in the reader's own timezone, which is why it is the
/// browser's job: the server has the instant and does not have the reader.
///
/// Break any part and the page still renders perfectly: there is simply no tooltip, on hover, with
/// nothing logged. So: the templates emit `data-at`, `localtime.js` reads it, and every page that
/// renders one loads the file.
#[test]
fn a_shorthand_duration_carries_the_instant_behind_it() {
    let script = std::fs::read_to_string(source("static/localtime.js")).expect("localtime.js");

    // The CALL, not any mention of the attribute: the first version of this asserted
    // `contains("[data-at]")` and matched the doc comment on `stamp` describing what it sweeps, so
    // it passed with the selector renamed. Fourth time in this codebase a lint has matched its own
    // prose; see the note on the theme selector.
    assert!(
        script.contains(r#"querySelectorAll("[data-at]")"#),
        "localtime.js no longer sweeps the attribute the templates emit"
    );
    assert!(
        script.contains("window.PunaTime"),
        "the formatter is not exported, so tracker.js cannot reach it for the cells it builds"
    );

    let mut stamped = 0;
    for path in templates() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
        let source = blank_comments(&raw);
        let count = source.matches("data-at=").count();
        if count == 0 {
            continue;
        }
        stamped += count;

        // The page has to actually load the helper, or the attribute is inert markup.
        assert!(
            raw.contains("/static/localtime.js"),
            "{}: renders {count} `data-at` cell(s) and never loads localtime.js, so the tooltips \
             silently never appear",
            label(&path)
        );
    }

    assert!(
        stamped >= 5,
        "only {stamped} timestamped cells found: this lint is no longer looking at anything"
    );
}

/// **A page that offers a creation control can explain a refusal of it.**
///
/// Three controls pass the creation gate: the upload link, the form that opens a room from a seed,
/// and the clone form on a room page. None of the pages carrying them is itself gated, so for as
/// long as the gate has existed a refused reader saw exactly the page everybody else saw, followed
/// the control, and got a `403` with an empty body and no explanation anywhere on it.
///
/// The fix is per page and so is the way it rots: a fourth creation control added later renders
/// unconditionally, works for everybody the gate admits, and sends everybody else back to the dead
/// end. Nothing about that fails, and it is invisible to whoever adds it, because they can create
/// rooms.
///
/// So this asserts only the weak form, which is the part a lint can see: a template offering one of
/// these controls names `creation_refused` somewhere. What it does with it is the render tests'
/// business, in `routes::generations` and `routes::rooms`.
#[test]
fn every_creation_control_sits_beside_the_reason_it_can_be_refused() {
    // The markup a reader clicks, not the routes: what is being pinned is that the PAGE knows the
    // gate exists. `action="/rooms"` rather than `/rooms`, which every "your rooms" link contains.
    const CONTROLS: &[(&str, &str)] = &[
        ("/generations/new", "the upload form"),
        (r#"action="/rooms""#, "opening a room from a seed"),
        ("/clone", "cloning a room"),
    ];

    let mut found = 0;
    for path in templates() {
        let source = std::fs::read_to_string(&path).expect("a readable template");
        for (marker, what) in CONTROLS {
            if !source.contains(marker) {
                continue;
            }
            found += 1;
            assert!(
                source.contains("creation_refused"),
                "{}: offers {what} without asking whether this reader may, so somebody the gate \
                 refuses is shown the control and answered with a bare 403",
                label(&path)
            );
        }
    }

    // A lint that finds nothing passes, and this one is a search for three strings.
    assert_eq!(
        found,
        CONTROLS.len(),
        "expected one page per creation control and found {found}: either a control moved or this \
         lint is looking at nothing"
    );
}

/// A template with every `{% include "x" %}` replaced by the file it names.
///
/// One level deep, which is all this codebase has. Anything unreadable is left as the include tag,
/// so a renamed partial shows up as a missing feature rather than as a silent pass.
fn expand_includes(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;

    while let Some(at) = rest.find("{% include \"") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "{% include \"".len()..];
        let Some((name, tail)) = after.split_once('"') else {
            out.push_str(rest);
            return out;
        };
        match std::fs::read_to_string(source_template(name)) {
            Ok(included) => out.push_str(&included),
            Err(_) => out.push_str(&rest[at..at + "{% include \"".len() + name.len()]),
        }
        rest = tail;
    }

    out.push_str(rest);
    out
}

fn source_template(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("templates")
        .join(relative)
}
