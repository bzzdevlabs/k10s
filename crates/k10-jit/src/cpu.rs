//! Optional CPU features the code generator may use.

/// Instruction-set extensions the generated code may rely on. A blob compiled with a feature enabled must only
/// run on CPUs that have it: `lzcnt`/`tzcnt` silently execute as `bsr`/`bsf` on CPUs without them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CpuFeatures {
    /// x86-64 `lzcnt` (ABM / LZCNT, CPUID 8000_0001h:ECX bit 5). Fallback: `bsr` + fix-up.
    pub lzcnt: bool,
    /// x86-64 `tzcnt` (BMI1, CPUID 7:EBX bit 3). Fallback: `bsf` + fix-up.
    pub tzcnt: bool,
    /// x86-64 `popcnt` (CPUID 1:ECX bit 23). Fallback: SWAR bit counting.
    pub popcnt: bool,
    /// AArch64 speculation barrier `sb` (FEAT_SB), used by the hardened indirect-branch modes. Fallback:
    /// `dsb nsh; isb`.
    pub sb: bool,
}

impl CpuFeatures {
    /// Plain x86-64 (SSE2 only) or Armv8.0-A: every optional instruction uses its fallback.
    pub const BASELINE: CpuFeatures = CpuFeatures { lzcnt: false, tzcnt: false, popcnt: false, sb: false };

    /// The features of the CPU we are running on (CPUID on x86-64; the kernel's hwcaps on AArch64 with `std`).
    /// [`CpuFeatures::BASELINE`] elsewhere.
    pub fn detect() -> CpuFeatures {
        #[cfg(target_arch = "x86_64")]
        {
            use core::arch::x86_64::{__cpuid, __cpuid_count};
            // SAFETY: CPUID is available on every x86-64 CPU and has no side effects.
            #[allow(unused_unsafe)]
            let (max, max_ext) = unsafe { (__cpuid(0).eax, __cpuid(0x8000_0000).eax) };
            #[allow(unused_unsafe)]
            let popcnt = max >= 1 && unsafe { __cpuid(1).ecx } & (1 << 23) != 0;
            #[allow(unused_unsafe)]
            let tzcnt = max >= 7 && unsafe { __cpuid_count(7, 0).ebx } & (1 << 3) != 0;
            #[allow(unused_unsafe)]
            let lzcnt = max_ext >= 0x8000_0001 && unsafe { __cpuid(0x8000_0001).ecx } & (1 << 5) != 0;
            CpuFeatures { lzcnt, tzcnt, popcnt, sb: false }
        }
        #[cfg(all(target_arch = "aarch64", feature = "std"))]
        {
            CpuFeatures { sb: std::arch::is_aarch64_feature_detected!("sb"), ..CpuFeatures::BASELINE }
        }
        #[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", feature = "std"))))]
        {
            CpuFeatures::BASELINE
        }
    }

    /// True if every feature enabled in `self` is also enabled in `cpu`.
    pub fn runs_on(&self, cpu: &CpuFeatures) -> bool {
        (!self.lzcnt || cpu.lzcnt) && (!self.tzcnt || cpu.tzcnt) && (!self.popcnt || cpu.popcnt) && (!self.sb || cpu.sb)
    }
}
