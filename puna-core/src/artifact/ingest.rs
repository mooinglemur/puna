//! Read a generation zip: validate it, and index what is inside.
//!
//! Runs in the WEB tier, synchronously, at upload time. That placement is the point: a seed that
//! cannot be parsed becomes a 400 on the upload form, not a room whose pod crashloops minutes
//! later with the reason buried in a container log.
//!
//! ## Matching patches to slots
//!
//! Mirrors what the reference implementation does in `WebHostLib/upload.py`, in the same order:
//!
//! 1. **The patch's own `archipelago.json`**, when the member is a zip container. It carries
//!    `player`, `player_name` and `game`, and it is authoritative. 91 of 100 patch members across
//!    13 real generation zips have one.
//! 2. **The `P<n>` component of the filename** otherwise, because several patch types are not
//!    containers at all -- `.apmanual` and `.apmc` are raw bytes, not zips.
//!
//! Puna differs from the reference in one respect, deliberately: the reference gates step 1 on
//! `AutoPatchRegister`, a registry populated by the apworlds a WebHost happens to have installed,
//! so an unrecognized extension falls through to filename parsing. Puna has no apworld registry,
//! so it simply tries every member as a zip. That is strictly more permissive and needs no
//! per-game knowledge.
//!
//! Two shapes make filename parsing harder than it looks, and both are normal:
//!
//! ```text
//! AP_14318265276849580066_P40_IronSquire_SMS.apsms                      underscores in the NAME
//! AP-14318265276849580066-P51-Matthias_KH2-19Apr2026-181027_0.6.7.zip   hyphens, name, date, version
//! ```
//!
//! `get_out_file_name_base` replaces spaces in player names with underscores, so neither separator
//! is a reliable field boundary. Player names are therefore never used to CHOOSE a slot -- only,
//! when a manifest supplies one, to detect a patch that belongs to a different generation.
//!
//! Anything attributable by neither route is REPORTED rather than guessed at: silently attaching a
//! patch to the wrong slot hands a player someone else's game.
//!
//! ## Which slots are kept
//!
//! Players and spectators; group slots (item links) are dropped. That is the reference's rule from
//! both ends: `process_multidata` skips `SlotType.group` and nothing else, and `MultiServer`
//! resolves a Connect through the multidata's `connect_names` with no slot-type filter at all
//! (`MultiServer.py:1880`). A spectator is a slot someone logs into -- it comes from a yaml, has a
//! name in `connect_names`, and watches the multiworld -- so it needs an owner, a claim link and a
//! tracker id like any other. It simply plays nothing, which is what `SlotKind` records.
//!
//! Groups are the opposite case: no yaml creates one, nothing connects as one, and the server
//! builds them from this same multidata, so a row for one would be unclaimable and unplayable.
//!
//! ## The load-time checks
//!
//! Parsing is not the whole question. pahoa runs `MultiData::validate` on the serve path, before
//! it binds its port, so a seed that parses and is *inconsistent* -- a hole in the locations
//! table, a connect name pointing at a slot with no world behind it, a group listing a member
//! that does not exist, a team this server cannot serve -- is a pod that exits at startup rather
//! than a room. That is the [`load_refusal`] check here, and it is pahoa's own function rather
//! than a transcription of it: the two must agree, and the only way to be sure they do is for
//! there to be one of them.
//!
//! Puna answers the structural half and deliberately not the version half; see [`load_refusal`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use pahoa_multidata::{MultiData, SlotType};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("not a readable zip archive: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("no .archipelago file in the archive; is this a generation output zip?")]
    NoMultidata,

    #[error("{0} .archipelago files in the archive; expected exactly one")]
    MultipleMultidata(usize),

    #[error("the .archipelago file could not be parsed: {0}")]
    Multidata(String),

    /// It parses, and a room would refuse to serve it. See [`load_refusal`].
    ///
    /// Separate from [`IngestError::Multidata`] because the two say different things to whoever
    /// uploaded it: that one means Puna could not read the file, this one means the file is
    /// internally inconsistent and the seed wants regenerating.
    #[error(
        "this seed will not load: {0}. \
         A room opened from it would exit at startup instead of serving, so it is refused here."
    )]
    WillNotLoad(String),

    #[error("archive is {size} bytes, over the {limit} byte limit")]
    TooLarge { size: u64, limit: u64 },

    #[error(
        "archive contains {member}, which looks like a game ROM. \
         Generation output never contains one, and distributing copyrighted ROMs is not something \
         this service will do. Upload the generation zip as produced, without added files."
    )]
    BannedFile { member: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What a slot is, for the slots Puna keeps.
///
/// Group slots have no variant on purpose: they are dropped at ingest rather than stored as a kind
/// with no owner, no patch and no way to connect. Making them unrepresentable here is what stops a
/// later route having to remember to filter them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Player,
    /// Connects, watches everything, plays nothing. The `Archipelago` pseudo-game in the reference
    /// (`worlds/generic`) marks its slot this way, and pahoa lets it connect like any other.
    Spectator,
}

/// One connectable slot, and the patch belonging to it if there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotEntry {
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
    pub kind: SlotKind,
    /// Path inside the zip. `None` for games that produce no patch, which is normal.
    pub patch_member: Option<String>,
    pub patch_size_bytes: Option<i64>,
}

/// Everything Puna records about a generation.
#[derive(Debug, Clone)]
pub struct GenerationMeta {
    pub sha256: [u8; 32],
    pub size_bytes: i64,
    pub seed_name: String,
    /// Total slots as pahoa counts them, INCLUDING spectator and group slots.
    ///
    /// This feeds the room's memory request, and pahoa sizes its outbound budget from
    /// `slot_info.len()` -- so counting only players here would under-request memory for a
    /// multiworld with item-link groups. `slots` below is the per-player list, which is a
    /// different question and deliberately a different number.
    pub slot_count: i32,
    pub locations: i64,
    /// Distinct games being PLAYED, so a spectator's `Archipelago` pseudo-game is not among them.
    /// This is the "12 games" a room page shows, and counting the watcher would be wrong there.
    pub games: Vec<String>,
    pub race_mode: bool,
    pub min_server_version: Option<String>,
    /// Path of the `.archipelago` inside the zip.
    pub multidata_member: String,
    pub spoiler_member: Option<String>,
    /// Connectable slots -- players and spectators -- in slot order.
    pub slots: Vec<SlotEntry>,
    /// Members that look like patches but could not be attributed to a slot.
    ///
    /// Surfaced to the uploader rather than dropped: a patch nobody can download is a player who
    /// cannot join, and finding that out at upload time is much cheaper than at game time.
    pub unmatched_patches: Vec<String>,
}

/// Members that are never patches.
fn is_ignorable(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with('/')
        || lower.ends_with(".archipelago")
        || lower.contains("spoiler")
        || lower.starts_with("yamls")
        || lower.contains("/yamls")
        || lower.ends_with(".txt")
}

/// Extensions that are whole game ROMs rather than patches, taken from `banned_extensions` in
/// `WebHostLib/upload.py`.
///
/// Patch extensions are all `.ap*` (`.apsms`, `.apgb`, ...), so none of these can match one -- a
/// bare `.sms` or `.gb` in a generation zip is a ROM someone added by hand. Puna stores what it is
/// given and serves it back per-slot, so accepting one would make it a ROM distributor.
const BANNED_EXTENSIONS: &[&str] = &[
    ".sfc", ".z64", ".n64", ".nes", ".smc", ".sms", ".gb", ".gbc", ".gba",
];

/// Does this member name a ROM?
///
/// Matched case-insensitively, unlike the reference's `str.endswith`, so `ROM.SFC` is caught too.
/// The reference is a web form where a rejected upload is retried; Puna's copy is content-addressed
/// onto a shared volume, so a miss here is a file that stays.
fn is_banned_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BANNED_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// What a patch's own `archipelago.json` says about it.
#[derive(Debug, serde::Deserialize)]
struct PatchManifest {
    player: Option<u32>,
    player_name: Option<String>,
}

/// Read `archipelago.json` from a patch that is itself a zip.
///
/// Returns `None` when the member is not a zip (`.apmanual` and friends) or carries no manifest.
fn read_patch_manifest(bytes: &[u8]) -> Option<PatchManifest> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut file = archive.by_name("archipelago.json").ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Is the byte at `i` a separator, or off either end of `s`?
fn is_separator_at(s: &[u8], i: usize) -> bool {
    match s.get(i) {
        None => true,
        Some(b) => matches!(b, b'_' | b'-' | b'.'),
    }
}

/// Pull the slot number out of a patch filename: the `P<n>` component.
///
/// `P<n>` IS the slot number. The canonical name comes from `MultiWorld.get_out_file_name_base` in
/// the reference implementation:
///
/// ```text
/// AP_{seed_name}_P{player}_{file_safe_player_name with spaces -> underscores}
/// ```
///
/// Note that player names therefore CONTAIN underscores (`IronSquire_SMS`, `octo_doge_SML2`), and
/// Factorio-style containers use hyphens with the game and version appended
/// (`AP-{seed}-P51-Matthias_KH2-19Apr2026-181027_0.6.7.zip`). So neither separator can be treated
/// as a reliable field boundary, and player names are not consulted at all -- a name can contain
/// separators, be a substring of another name, or repeat.
///
/// VERIFIED against the reference: `WebHostLib/upload.py` reads `archipelago.json` for registered
/// container types and otherwise takes the third `_`-delimited component. Run over 13 real modern
/// generation zips -- 100 patch members, 91 of them carrying a manifest -- this function agreed
/// with the reference on all 100, and with the manifest's authoritative `player` on all 91.
///
/// The delimiter requirement is defensive rather than necessary for well-formed output. It matters
/// for names where a `P`-plus-digits sequence appears outside the slot field, which a since-fixed
/// Minecraft apworld bug used to produce (`AP_bciP6tGNR-GbEx0RqU-2Bg_P16_...`, where a bare
/// `P(\d+)` search finds the seed's `P6` first). Requiring separators on both sides reads such a
/// name correctly, and two differing delimited components make it refuse rather than guess.
fn slot_from_filename(name: &str) -> Option<u32> {
    let stem = name.rsplit('/').next().unwrap_or(name);
    // Trim the extension so a trailing `.apsm` cannot be mistaken for a delimiter run.
    let stem = stem.rsplit_once('.').map_or(stem, |(head, _)| head);
    let bytes = stem.as_bytes();

    let mut found: Option<u32> = None;
    for (i, _) in stem.match_indices('P') {
        if !(i == 0 || is_separator_at(bytes, i - 1)) {
            continue;
        }
        let digits: String = stem[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() || !is_separator_at(bytes, i + 1 + digits.len()) {
            continue;
        }
        let Ok(slot) = digits.parse::<u32>() else {
            continue;
        };
        // Two delimited P-components in one name is not something the corpus contains, but if it
        // ever happens the filename does not say which is the slot, so refuse.
        if found.is_some_and(|prev| prev != slot) {
            return None;
        }
        found = Some(slot);
    }
    found
}

/// Why a room would refuse to load this seed, or `None` if it would serve it.
///
/// This is **pahoa's `MultiData::validate`, called rather than transcribed**. It is the same
/// function the room runs before it binds its port, so the answer here and the answer there cannot
/// disagree -- which a second implementation of the reference's `NetUtils.py:449-506` checks would
/// eventually manage to do. Puna already links the parser for the same reason.
///
/// It covers: a locations table with a hole in its slot ids or a duplicated location, a
/// `connect_names` entry pointing at a slot with no `slot_info` (a name somebody could
/// authenticate as with no world behind it), a group listing a member that does not exist, and a
/// slot on a team other than 0 -- which nothing can generate and neither server can serve.
///
/// **The version arm is deliberately made vacuous, by handing `validate` the seed's own floor.**
/// That arm asks "is *this server* new enough", and Puna is not the server: the room's version is
/// whatever `PUNA_PAHOA_IMAGE` resolves to, which only the orchestrator names and only the probe
/// can read back -- and neither is available at upload. The alternative is a version constant
/// transcribed from another repository, and its failure runs the wrong way: a constant that goes
/// stale LOW makes Puna refuse a seed the room would happily serve, blaming the seed for a number
/// in Puna's source. A seed genuinely demanding a newer server is left to the room, which refuses
/// it by name on stderr. The honest fix is for `pahoa-multidata` to export `SERVER_VERSION`, which
/// today lives a crate above it; that is asked for in the handoff.
///
/// The margin makes that trade cheap rather than merely defensible: every seed in the corpus
/// demands 0.5.0 while pahoa reports 0.6.8, so this arm binds only on a seed from the future.
pub fn load_refusal(data: &MultiData) -> Option<String> {
    data.validate(data.minimum_server_version)
        .err()
        .map(|e| e.to_string())
}

/// [`load_refusal`], for a seed already promoted to the volume.
///
/// The upload check is not the whole answer, and the reason is not the rows that predate it -- it
/// is that **these checks change**. They live in `pahoa-multidata` at a pinned rev, and pahoa
/// tightening them (or fixing one, as it just did for spectators) means every generation on the
/// volume was last checked under the previous rules. A room opened from one of them is the case
/// the upload check cannot cover, so it is checked again where a room is opened.
pub fn seed_refusal(seed: &[u8]) -> Result<Option<String>, IngestError> {
    let data = MultiData::parse(seed).map_err(|e| IngestError::Multidata(e.to_string()))?;
    Ok(load_refusal(&data))
}

/// Inspect a generation zip.
pub fn inspect(bytes: &[u8], size_limit: u64) -> Result<GenerationMeta, IngestError> {
    let size = bytes.len() as u64;
    if size > size_limit {
        return Err(IngestError::TooLarge {
            size,
            limit: size_limit,
        });
    }

    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;

    // Locate the multidata, and refuse ROMs. Both are decided from the central directory alone,
    // before a single member is decompressed, so a banned archive is rejected without Puna ever
    // holding the file's contents in memory.
    let mut multidata_members: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file.name().to_string();
        if is_banned_file(&name) {
            return Err(IngestError::BannedFile { member: name });
        }
        if name.to_ascii_lowercase().ends_with(".archipelago") {
            multidata_members.push(name);
        }
    }
    match multidata_members.len() {
        0 => return Err(IngestError::NoMultidata),
        1 => {}
        n => return Err(IngestError::MultipleMultidata(n)),
    }
    let multidata_member = multidata_members.remove(0);

    let multidata_bytes = {
        let mut file = archive.by_name(&multidata_member)?;
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)?;
        buf
    };

    let data =
        MultiData::parse(&multidata_bytes).map_err(|e| IngestError::Multidata(e.to_string()))?;

    // Before anything is attributed, because a seed no room will load has nothing worth indexing,
    // and because the whole point of doing this at upload is that it is a sentence on a form
    // rather than a pod exiting at startup with the reason in a container log.
    if let Some(reason) = load_refusal(&data) {
        return Err(IngestError::WillNotLoad(reason));
    }

    // Connectable slots: players and spectators, groups dropped. See the module docs: a spectator
    // that is missing here is a spectator with no password in a `per_slot` room. Groups still count
    // toward pahoa's memory budget, which is why `slot_count` is taken separately below.
    let connectable: BTreeMap<u32, (String, String, SlotKind)> = data
        .slot_info
        .iter()
        .filter_map(|(n, s)| {
            let kind = match s.slot_type {
                SlotType::Player => SlotKind::Player,
                SlotType::Spectator => SlotKind::Spectator,
                SlotType::Group => return None,
            };
            Some((*n, (s.name.clone(), s.game.clone(), kind)))
        })
        .collect();

    let mut patch_for_slot: BTreeMap<u32, (String, i64)> = BTreeMap::new();
    let mut unmatched_patches: Vec<String> = Vec::new();
    let mut spoiler_member: Option<String> = None;

    for i in 0..archive.len() {
        let (name, member_size) = {
            let file = archive.by_index(i)?;
            (file.name().to_string(), file.size())
        };

        if name.to_ascii_lowercase().contains("spoiler") {
            spoiler_member.get_or_insert(name);
            continue;
        }
        if is_ignorable(&name) {
            continue;
        }

        let member_bytes = {
            let mut file = archive.by_index(i)?;
            let mut buf = Vec::with_capacity(member_size as usize);
            file.read_to_end(&mut buf)?;
            buf
        };

        // The patch's own manifest first: authoritative, and immune to every naming convention.
        // The filename's `P<n>` is the fallback for patch types that are not zips, such as
        // `.apmanual`. Player names are never used to CHOOSE a slot.
        let manifest = read_patch_manifest(&member_bytes);
        let slot = manifest
            .as_ref()
            .and_then(|m| m.player)
            .or_else(|| slot_from_filename(&name));

        // A manifest name that disagrees with the multidata is the signature of patches from a
        // DIFFERENT generation being bundled in: the slot number would still resolve, and the
        // player would receive a patch for someone else's world in a seed they are not playing.
        // Cheap to detect here, effectively undiagnosable later.
        if let (Some(slot), Some(claimed)) = (
            slot,
            manifest.as_ref().and_then(|m| m.player_name.as_deref()),
        ) && let Some((expected, _, _)) = connectable.get(&slot)
            && claimed != expected
        {
            tracing::warn!(
                member = %name,
                slot,
                %claimed,
                %expected,
                "patch manifest names a different player than the seed does; not attributing it"
            );
            unmatched_patches.push(name);
            continue;
        }

        // Attributable only to a PLAYER slot. A patch that resolves to a spectator or to a slot
        // this seed does not have is reported, never attached: a spectator plays nothing, so a
        // patch naming one is evidence the file came from somewhere else.
        let target = slot.filter(|s| matches!(connectable.get(s), Some((_, _, SlotKind::Player))));
        match target {
            Some(slot) => {
                patch_for_slot.insert(slot, (name, member_size as i64));
            }
            None => unmatched_patches.push(name),
        }
    }

    let slots: Vec<SlotEntry> = connectable
        .iter()
        .map(|(number, (player_name, game, kind))| {
            let patch = patch_for_slot.get(number);
            SlotEntry {
                slot_number: *number as i32,
                player_name: player_name.clone(),
                game: game.clone(),
                kind: *kind,
                patch_member: patch.map(|(n, _)| n.clone()),
                patch_size_bytes: patch.map(|(_, s)| *s),
            }
        })
        .collect();

    let games: Vec<String> = connectable
        .values()
        .filter(|(_, _, kind)| *kind == SlotKind::Player)
        .map(|(_, game, _)| game.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    Ok(GenerationMeta {
        sha256,
        size_bytes: size as i64,
        seed_name: data.seed_name.clone(),
        slot_count: data.slot_info.len() as i32,
        locations: data.locations.len() as i64,
        games,
        race_mode: data.race_mode,
        min_server_version: Some(data.minimum_server_version.to_string()),
        multidata_member,
        spoiler_member,
        slots,
        unmatched_patches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case below is a filename taken from real Archipelago generation output.
    #[test]
    fn slot_numbers_come_from_real_filenames() {
        for (name, expected) in [
            // Canonical form.
            ("AP_14318265276849580066_P31_FrootRoop-SM.apsm", Some(31)),
            ("AP_14318265276849580066_P55_Minecraft.apmc", Some(55)),
            // UNDERSCORES IN PLAYER NAMES: the normal case, since get_out_file_name_base
            // replaces spaces with underscores. No separator is a reliable field boundary.
            ("AP_14318265276849580066_P40_IronSquire_SMS.apsms", Some(40)),
            (
                "AP_14318265276849580066_P64_octo_doge_SML2.apsml2",
                Some(64),
            ),
            (
                "AP_14318265276849580066_P94_TEC_FireRed.apleafgreen",
                Some(94),
            ),
            (
                "AP_14318265276849580066_P28_Fabricator_DK64.chunky",
                Some(28),
            ),
            // One- and two-digit slots, so the digit run is not assumed to be a single character.
            ("AP_14318265276849580066_P7_CiaPlinkTTP.aplttp", Some(7)),
            ("AP_14318265276849580066_P96_Xeno_WW.aptww", Some(96)),
            // Factorio: hyphen separators with the version appended.
            (
                "AP-14318265276849580066-P33-gr3at-Factorio_0.6.7.zip",
                Some(33),
            ),
            // Factorio again, with underscores AND a date inside the name.
            (
                "AP-14318265276849580066-P51-Matthias_KH2-19Apr2026-181027_0.6.7.zip",
                Some(51),
            ),
        ] {
            assert_eq!(slot_from_filename(name), expected, "{name}");
        }
    }

    /// Tolerance for malformed LEGACY names, kept deliberately and labeled as such.
    ///
    /// A since-fixed Minecraft apworld bug emitted seeds containing `_` and `P`-plus-digits. The
    /// reference implementation rejects those outright, which is correct on its part -- this is a
    /// tolerance, not a correctness advantage. Pinned so it cannot regress into a silent
    /// mis-attribution, which is the outcome that actually matters.
    #[test]
    fn a_malformed_legacy_name_is_read_or_refused_never_misattributed() {
        // Only one delimited P-component, so it resolves, and to the right slot.
        assert_eq!(
            slot_from_filename("AP_bciP6tGNR-GbEx0RqU-2Bg_P16_ExamplePlayerMC.apmc"),
            Some(16),
            "a bare P(\\d+) search would pick the seed's embedded P6"
        );
        // Two DIFFERENT delimited components: nothing in the name says which is the slot, so it
        // refuses and the member is reported instead of guessed at.
        assert_eq!(slot_from_filename("AP_seed-P9-x_P16_Name.apmc"), None);
    }

    #[test]
    fn a_filename_with_no_slot_component_is_reported_not_guessed() {
        // Both real. Neither carries a slot number, so neither may be attributed from its name:
        // if such a patch is not a zip with a manifest, it lands in `unmatched_patches`.
        assert_eq!(
            slot_from_filename("AP_vZgp2aaNToWM3RknI8UoFA_SP.apsm64ex"),
            None
        );
        assert_eq!(slot_from_filename("AP-20240224.zip"), None);
    }

    #[test]
    fn a_bare_p_without_digits_is_not_a_slot() {
        assert_eq!(slot_from_filename("AP_123_P_nodigits.apsm"), None);
        assert_eq!(slot_from_filename("AP_123_Player_name.apsm"), None);
    }

    #[test]
    fn ignorable_members_are_skipped() {
        assert!(is_ignorable("AP_123.archipelago"));
        assert!(is_ignorable("AP_123_Spoiler.txt"));
        assert!(is_ignorable("yamls-2025-10-18.zip"));
        assert!(is_ignorable("some/dir/"));
        assert!(!is_ignorable("AP_123_P1_Sam.apsm"));
    }

    /// Every extension the reference bans, plus the shapes that must NOT be caught.
    #[test]
    fn roms_are_recognized_and_patches_are_not() {
        for rom in [
            "zelda.sfc",
            "mario.smc",
            "oot.z64",
            "banjo.n64",
            "smb.nes",
            "sonic.sms",
            "tetris.gb",
            "oracle.gbc",
            "metroid.gba",
            // Case is the reference's blind spot: str.endswith is case-sensitive.
            "LTTP.SFC",
            "roms/Super Metroid.Smc",
        ] {
            assert!(is_banned_file(rom), "{rom}");
        }

        // Real patch members. Every patch extension is `.ap*`, so none can collide with a ROM
        // extension, but `.apsms` next to a banned `.sms` is close enough to be worth pinning.
        for ok in [
            "AP_14318265276849580066_P40_IronSquire_SMS.apsms",
            "AP_14318265276849580066_P55_Minecraft.apmc",
            "AP_14318265276849580066_P28_Fabricator_DK64.chunky",
            "AP_14318265276849580066.archipelago",
            "AP_14318265276849580066_Spoiler.txt",
        ] {
            assert!(!is_banned_file(ok), "{ok}");
        }
    }

    #[test]
    fn a_rom_in_the_archive_rejects_the_whole_upload() {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("AP_123.archipelago", opts).unwrap();
            zip.start_file("Zelda no Densetsu.sfc", opts).unwrap();
            zip.finish().unwrap();
        }

        // Note this outranks the multidata being present and parseable: the archive is refused
        // before any member is read, so nothing about a ROM's neighbors can rescue it.
        let err = inspect(&buf, 1 << 20).unwrap_err();
        match err {
            IngestError::BannedFile { member } => assert_eq!(member, "Zelda no Densetsu.sfc"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_non_zip_is_rejected_clearly() {
        let err = inspect(b"not a zip at all", 1024).unwrap_err();
        assert!(matches!(err, IngestError::Zip(_)), "got {err:?}");
    }

    #[test]
    fn oversized_input_is_rejected_before_parsing() {
        let err = inspect(&[0u8; 128], 64).unwrap_err();
        assert!(matches!(err, IngestError::TooLarge { .. }), "got {err:?}");
    }

    /// **`inspect` must actually call `load_refusal`**, and this is a source lint because nothing
    /// else here can reach that.
    ///
    /// The refusal cases are covered against a real seed in `tests/ingest.rs`, by mutating a
    /// parsed `MultiData` -- but a test that calls `load_refusal` directly keeps passing when the
    /// call site is deleted, and there is no other symptom: a malformed seed simply uploads,
    /// indexes cleanly, and becomes a room whose pod exits at startup. The whole feature is the
    /// call site, so the call site is what is pinned.
    ///
    /// Re-pickling a mutated multidata into a zip would test this properly and is not available:
    /// `MultiData::parse` is a decoder with no encoder beside it.
    #[test]
    fn inspect_refuses_a_seed_that_would_not_load() {
        let source = include_str!("ingest.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a non-test half");

        let call = source
            .find("if let Some(reason) = load_refusal(&data)")
            .expect("`inspect` no longer runs the load-time checks a room runs before it starts");
        let parse = source
            .find("MultiData::parse(&multidata_bytes)")
            .expect("the parse call was renamed; re-point this lint rather than deleting it");
        // Ordering is not incidental: the checks read the parsed seed, and everything after them
        // (patch attribution, the slot list, the games) is work on a seed no room will load.
        assert!(
            call > parse,
            "the load-time checks must run on the parsed seed"
        );
        assert!(
            source[call..].contains("IngestError::WillNotLoad"),
            "the refusal must reach the uploader as a rejection, not a log line"
        );
    }
}
