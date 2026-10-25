//! Cluster-rolled updates of the native kernel (metal/README.md, "Updates"): Tier 1's rollout (k10_mesh::rollout:
//! waves ranked by image and node id, a failed image stops it everywhere) with K10S-DATA's kernel slots in place of
//! K10S-BOOT's UKIs. k10boot tells which kernel it booted (module `k10s.image`): the kernel file of K10S-BOOT or a
//! slot, on trial or not, and the keys it trusted. The node reports its image in its gossip record; when its turn
//! comes it verifies the image of its platform against those keys, writes it into the slot it does not run (data
//! flushed, then the superblock arms the trial) and reboots into it. A trial commits once the node holds and applied
//! the cluster's desired state and, if it had peers, sees one again, for `k10.os_commit_grace` seconds (30); it is
//! abandoned after `k10.os_trial` seconds (600), and when k10boot fell back to the committed kernel (two boots that
//! never got healthy): the image is recorded as failed (`failed` of the store's `.native` namespace), which halts
//! the rollout. A power cut at any step leaves the committed kernel booting, or the armed trial being counted.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use k10_bootproto::{BootedImage, MOD_IMAGE};
use k10_kvlog::layout;
use k10_mesh::rollout::{self, Step};
use k10_mesh::sha256::{sha256, to_hex};
use k10_mesh::{osimage, Member, NodeId, OsInfo, OsPlan};

use crate::kprintln;
use crate::pvh::BootInfo;
use crate::sync::Global;
use crate::time;

/// `.native` key: the hashes of the images that failed here (newest last).
const FAILED: &[u8] = b"failed";
const MAX_FAILED: usize = 16;
/// A started node that had peers follows the rollout only after this long (it judges its turn on the cluster's
/// membership, not on its own record alone).
const SETTLE_NS: u64 = 15_000_000_000;
/// The platform of this kernel's images.
const PLATFORM: &str = "x86_64-native";

struct Agent {
    image: BootedImage,
    committed: bool,
    grace_ns: u64,
    trial_deadline: u64,
    good_since: Option<u64>,
    /// An install was attempted for this image (once per boot).
    asked: Option<[u8; 32]>,
    started: u64,
    step: String,
}

static AGENT: Global<Option<Agent>> = Global::new(None);

fn letter(slot: u8) -> char {
    (b'A' + slot) as char
}

fn failed_list() -> Vec<[u8; 32]> {
    crate::storage::native_get(FAILED).unwrap_or_default().chunks_exact(32).map(|c| c.try_into().unwrap()).collect()
}

fn record_failed(h: [u8; 32]) {
    let mut l = failed_list();
    l.retain(|x| *x != h);
    l.push(h);
    let keep = l.len().saturating_sub(MAX_FAILED);
    let bytes: Vec<u8> = l[keep..].concat();
    if let Err(e) = crate::storage::native_set(FAILED, Some(&bytes)) {
        kprintln!("os: cannot record image {} as failed: {e}", to_hex(&h[..4]));
    }
}

/// Reads k10boot's `k10s.image` module (after storage::init) and settles a trial k10boot gave up on.
pub fn init(bi: &BootInfo) {
    let Some((_, m)) = bi.module(MOD_IMAGE) else { return };
    // SAFETY: the module is reserved until released here.
    let image = BootedImage::decode(unsafe { m.bytes() });
    crate::mem::frames::release_module(m.paddr, m.size);
    let Some(image) = image else {
        return kprintln!("os: the loader's k10s.image module is malformed: updates are off");
    };
    let secs = |name: &str, default: u64| bi.arg(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(default) * 1_000_000_000;
    if let Some(sb) = crate::storage::superblock() {
        if let Some(t) = sb.trial {
            if !(image.trial && image.source == 1 + t) {
                kprintln!("os: the trial kernel of slot {} did not come up: recorded as failed", letter(t));
                record_failed(sb.slots[t as usize].sha256);
                if let Err(e) = crate::storage::set_boot(sb.committed, None) {
                    kprintln!("os: {e}");
                }
            }
        }
    }
    let from = match image.source {
        0 => String::from("K10S-BOOT"),
        s => format!("slot {}{}", letter(s - 1), if image.trial { " (trial)" } else { "" }),
    };
    kprintln!("os: {} {} ({}) from {from}", image.platform(), image.version(), to_hex(&image.sha256[..4]));
    *AGENT.borrow_mut() = Some(Agent {
        committed: !image.trial,
        image,
        grace_ns: secs("k10.os_commit_grace", 30),
        trial_deadline: time::now_ns() + secs("k10.os_trial", 600),
        good_since: None,
        asked: None,
        started: time::now_ns(),
        step: String::new(),
    });
}

/// This node's OS report for gossip (`None`: not booted by k10boot).
pub fn info() -> Option<OsInfo> {
    let g = AGENT.borrow();
    let a = g.as_ref()?;
    Some(OsInfo {
        platform: a.image.platform().into(),
        image: OsInfo::id(&a.image.sha256),
        committed: a.committed,
        updatable: crate::storage::available() && a.image.platform() == PLATFORM,
        failed: failed_list().last().map_or([0; 8], OsInfo::id),
    })
}

/// Booted on trial (k10boot's boot counter is cleared at the commit, not once healthy).
pub fn on_trial() -> bool {
    AGENT.borrow().as_ref().is_some_and(|a| !a.committed)
}

/// The image of this node's platform that `plan` rolls out, when it is not the running one or a failed one: the
/// cluster fetches and keeps it.
pub fn wanted(plan: &OsPlan) -> Option<[u8; 32]> {
    let g = AGENT.borrow();
    let a = g.as_ref()?;
    let h = *plan.image_for(a.image.platform())?;
    (h != a.image.sha256 && !failed_list().contains(&h)).then_some(h)
}

/// The rollout's state for `/_k10s/status`.
pub fn step_text() -> String {
    AGENT.borrow().as_ref().map_or_else(String::new, |a| a.step.clone())
}

fn describe(step: &Step) -> String {
    let short = |h: &[u8; 32]| to_hex(&h[..4]);
    match step {
        Step::Idle => "idle".into(),
        Step::Update { image, wave } => format!("installing {} (wave {wave})", short(image)),
        Step::Wait { image, wave, pending } => format!("{} waits for {pending} node(s) of earlier waves (wave {wave})", short(image)),
        Step::Halted { image, node } => format!("rollout of {} stopped: node {node} failed to commit it", short(image)),
        Step::Unknown { image, members } => format!("{} waits for the OS report of {members} member(s)", short(image)),
    }
}

/// Verifies `blob` (image `h`) and writes it into the slot this kernel does not run; returns the slot.
fn install(a: &Agent, blob: &[u8], h: [u8; 32]) -> Result<u8, String> {
    if sha256(blob) != h {
        return Err("the blob does not match its hash".into());
    }
    let info = osimage::verify(blob, a.image.keys()).map_err(|e| format!("refused: {e}"))?;
    let platform = format!("{}-{}", info.meta.arch, info.meta.profile);
    if platform != a.image.platform() {
        return Err(format!("an image for {platform}, this node runs {}", a.image.platform()));
    }
    if blob.len() as u64 > layout::SLOT_BYTES {
        return Err(format!("{} bytes, a slot holds {}", blob.len(), layout::SLOT_BYTES));
    }
    let sb = crate::storage::superblock().ok_or("no K10S-DATA")?;
    if sb.trial.is_some() {
        return Err("a trial is armed".into());
    }
    let running = a.image.source.checked_sub(1);
    let slot = [0u8, 1].into_iter().find(|&s| Some(s) != running && Some(s) != sb.committed).ok_or("no free slot")?;
    kprintln!("os: verified {} ({} {}, signed by {}): writing slot {}", to_hex(&h[..4]), platform, info.meta.version, to_hex(&info.key[..4]), letter(slot));
    crate::storage::install_slot(slot, blob, h)?;
    Ok(slot)
}

/// Called by the cluster about every second: commits or abandons a trial, follows the rollout. `ready`: the node
/// holds and applied the cluster's desired state and, if it expects peers, sees one. Returns the new gossip report
/// when it changed.
pub fn tick(plan: &OsPlan, members: &[Member], me: NodeId, ready: bool, expects_peers: bool, blob: &dyn Fn(&[u8; 32]) -> Option<Vec<u8>>) -> Option<OsInfo> {
    let mut g = AGENT.borrow_mut();
    let a = g.as_mut()?;
    let now = time::now_ns();
    if crate::power::requested().is_some() {
        return None;
    }
    if !a.committed {
        let slot = a.image.source.checked_sub(1)?;
        if now > a.trial_deadline {
            kprintln!("os: the trial of {} did not commit in time: back to the committed kernel", to_hex(&a.image.sha256[..4]));
            let sha = a.image.sha256;
            drop(g);
            record_failed(sha);
            let committed = crate::storage::superblock().and_then(|sb| sb.committed);
            if let Err(e) = crate::storage::set_boot(committed, None) {
                kprintln!("os: {e}");
            }
            crate::power::request(true);
            return info();
        }
        if !ready {
            a.good_since = None;
            return None;
        }
        let since = *a.good_since.get_or_insert(now);
        if now - since < a.grace_ns {
            return None;
        }
        // The counter first: committed with the trial's count left, the kernel would be refused as never healthy (a
        // power cut between the two only counts the trial's boots again).
        if let Err(e) = crate::efi::clear_boot_counter() {
            kprintln!("os: k10boot's boot counter is not cleared ({e}): slot {} not committed", letter(slot));
            return None;
        }
        match crate::storage::set_boot(Some(slot), None) {
            Ok(()) => {
                a.committed = true;
                kprintln!("os: slot {} ({}) committed", letter(slot), to_hex(&a.image.sha256[..4]));
                drop(g);
                return info();
            }
            Err(e) => {
                kprintln!("os: committing slot {}: {e}", letter(slot));
                return None;
            }
        }
    }
    if !crate::storage::available() || a.image.platform() != PLATFORM {
        return None;
    }
    if expects_peers && (now < a.started + SETTLE_NS || !members.iter().any(|m| m.id != me && m.is_live())) {
        return None;
    }
    let step = rollout::step(plan, members, me);
    let text = describe(&step);
    if text != a.step {
        if step != Step::Idle || !a.step.is_empty() {
            kprintln!("os: {text}");
        }
        a.step = text;
    }
    let Step::Update { image, .. } = step else { return None };
    if a.asked == Some(image) {
        return None;
    }
    let b = blob(&image)?;
    a.asked = Some(image);
    match install(a, &b, image) {
        Ok(slot) => {
            kprintln!("os: rebooting into slot {} on trial", letter(slot));
            crate::power::request(true);
            None
        }
        Err(e) => {
            kprintln!("os: refusing {}: {e}", to_hex(&image[..4]));
            drop(g);
            record_failed(image);
            info()
        }
    }
}
