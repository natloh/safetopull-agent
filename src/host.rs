//! Facts about the machine the agent runs on. Nothing here identifies the host.

use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct HostFacts {
    pub arch: String,
    /// `None` when the platform does not expose CPU flags, which makes CPU requirements "unverified".
    pub cpu_flags: Option<BTreeSet<String>>,
}

impl HostFacts {
    pub fn detect() -> Self {
        Self {
            arch: docker_arch(std::env::consts::ARCH).to_owned(),
            cpu_flags: std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|text| parse_cpu_flags(&text)),
        }
    }
}

pub fn docker_arch(rust_arch: &str) -> &str {
    match rust_arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

pub fn parse_cpu_flags(cpuinfo: &str) -> Option<BTreeSet<String>> {
    cpuinfo
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| matches!(name.trim(), "flags" | "Features"))
        .map(|(_, flags)| flags.split_whitespace().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_flags_from_x86_and_arm_cpuinfo() {
        let x86 = "processor\t: 0\nflags\t\t: fpu sse4_2 avx avx2\nbogomips\t: 1\n";
        let arm = "processor\t: 0\nFeatures\t: fp asimd aes\n";

        assert!(parse_cpu_flags(x86).unwrap().contains("avx2"));
        assert!(parse_cpu_flags(arm).unwrap().contains("asimd"));
        assert_eq!(parse_cpu_flags("model name: nothing useful"), None);
    }

    #[test]
    fn maps_rust_architectures_to_image_architectures() {
        assert_eq!(docker_arch("x86_64"), "amd64");
        assert_eq!(docker_arch("aarch64"), "arm64");
        assert_eq!(docker_arch("riscv64"), "riscv64");
    }
}
