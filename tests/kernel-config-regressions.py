#!/usr/bin/env python3
"""Keep reported guest-kernel requirements in both architecture inputs.

This checks configuration inputs, not guest boot or application compatibility.
The kernel build separately rejects requested options missing from .config.
"""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
KERNEL = ROOT / "guest/kernel"


def options(text):
    return dict(re.findall(r"^(CONFIG_[A-Z0-9_]+)=(\S+)$", text, re.MULTILINE))


class GuestKernelRequirements(unittest.TestCase):
    def test_reported_requirements_on_both_architectures(self):
        base = (KERNEL / "asagiri.config").read_text()
        # Match build.sh's removal of Arm-only options before the x86 overlay.
        x86 = re.sub(r"^CONFIG_(?:(?:ARM|ARM64)_.*|(?:RTC_DRV_PL031|PCI_HOST_GENERIC)=.*)\n",
                     "", base, flags=re.MULTILINE)
        requirements = {
            14: "NFT_REJECT NFT_REJECT_INET NFT_REJECT_IPV4 NFT_REJECT_IPV6 "
                "NFT_QUEUE NFT_CT NF_CONNTRACK_LABELS NFT_SOCKET NFT_TPROXY "
                "NFT_LOG NFT_LIMIT NFT_QUOTA",
            38: "BPF_SYSCALL BPF_JIT PERF_EVENTS DEBUG_INFO_BTF",
            41: "TASK_IO_ACCOUNTING",
            72: "FUSE_FS USER_NS",
        }
        for arch, text in (("arm64", base), ("x86_64", x86 + (KERNEL / "asagiri-x86_64.config").read_text())):
            config = options(text)
            for issue, names in requirements.items():
                for name in names.split():
                    with self.subTest(arch=arch, issue=issue, option=name):
                        self.assertEqual(config.get("CONFIG_" + name), "y")


if __name__ == "__main__":
    unittest.main()
