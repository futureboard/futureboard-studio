use serde::{Deserialize, Serialize};

use crate::stems::StemKind;

/// Offline source-separation model identifiers exposed in the Stem Extractor UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StemModel {
    /// Classic 4-stem MDX-NET pack (Kuielab A: vocals / drums / bass / other).
    #[default]
    MdxNet,
    /// MDX-NET karaoke variant (vocals + instrumental).
    MdxNetKaraoke,
    /// UVR MDX-NET Main (vocals + instrumental).
    MdxNetMain,
    /// Fine-tuned vocal MDX-NET (vocals + instrumental residual).
    MdxNetVocFt,
    /// High-quality instrumental MDX-NET (instrumental + vocals residual).
    MdxNetInstHq,
    /// Kim Vocal 2 (vocals + instrumental residual).
    KimVocal,
    /// Kim Instrumental (instrumental + vocals residual).
    KimInst,
    /// HT-Demucs single-file hybrid transformer (4-stem: drums, bass, other,
    /// vocals). Time-domain model, distinct from the MDX-NET STFT family.
    HtDemucs,
}

impl StemModel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MdxNet => "mdx-net",
            Self::MdxNetKaraoke => "mdx-net-karaoke",
            Self::MdxNetMain => "mdx-net-main",
            Self::MdxNetVocFt => "mdx-net-voc-ft",
            Self::MdxNetInstHq => "mdx-net-inst-hq",
            Self::KimVocal => "kim-vocal",
            Self::KimInst => "kim-inst",
            Self::HtDemucs => "htdemucs",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MdxNet => "MDX-NET",
            Self::MdxNetKaraoke => "MDX-NET Karaoke",
            Self::MdxNetMain => "MDX-NET Main",
            Self::MdxNetVocFt => "MDX-NET Voc FT",
            Self::MdxNetInstHq => "MDX-NET Inst HQ",
            Self::KimVocal => "Kim Vocal 2",
            Self::KimInst => "Kim Instrumental",
            Self::HtDemucs => "HT-Demucs",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::MdxNet => "4-stem Kuielab A: vocals, drums, bass, other",
            Self::MdxNetKaraoke => "2-stem karaoke: vocals and instrumental",
            Self::MdxNetMain => "2-stem UVR Main: vocals and instrumental",
            Self::MdxNetVocFt => "2-stem vocal fine-tune: vocals and instrumental",
            Self::MdxNetInstHq => "2-stem instrumental HQ: instrumental and vocals",
            Self::KimVocal => "2-stem Kim Vocal 2: vocals and instrumental",
            Self::KimInst => "2-stem Kim Instrumental: instrumental and vocals",
            Self::HtDemucs => "4-stem HT-Demucs: drums, bass, other, vocals",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mdx-net" | "mdxnet" | "MDX-NET" => Some(Self::MdxNet),
            "mdx-net-karaoke" | "mdxnet-karaoke" | "MDX-NET Karaoke" => Some(Self::MdxNetKaraoke),
            "mdx-net-main" | "mdxnet-main" | "MDX-NET Main" => Some(Self::MdxNetMain),
            "mdx-net-voc-ft" | "mdxnet-voc-ft" | "MDX-NET Voc FT" => Some(Self::MdxNetVocFt),
            "mdx-net-inst-hq" | "mdxnet-inst-hq" | "MDX-NET Inst HQ" => Some(Self::MdxNetInstHq),
            "kim-vocal" | "kim-vocal-2" | "Kim Vocal 2" => Some(Self::KimVocal),
            "kim-inst" | "kim-instrumental" | "Kim Instrumental" => Some(Self::KimInst),
            "htdemucs" | "ht-demucs" | "HT-Demucs" | "demucs" => Some(Self::HtDemucs),
            _ => None,
        }
    }

    pub fn default_stems(self) -> &'static [StemKind] {
        match self {
            Self::MdxNet | Self::HtDemucs => &[
                StemKind::Vocals,
                StemKind::Drums,
                StemKind::Bass,
                StemKind::Other,
            ],
            Self::MdxNetKaraoke
            | Self::MdxNetMain
            | Self::MdxNetVocFt
            | Self::MdxNetInstHq
            | Self::KimVocal
            | Self::KimInst => &[StemKind::Vocals, StemKind::Instrumental],
        }
    }

    pub fn supports_stem(self, stem: StemKind) -> bool {
        self.default_stems().contains(&stem)
    }

    /// Downloadable ONNX weight package for this model.
    ///
    /// The MDX-NET family is hosted on the public UVR model release; HT-Demucs
    /// is hosted on the StemSplit Hugging Face repository.
    pub fn package(self) -> StemModelPackage {
        use crate::download::{HTDEMUCS_MODEL_BASE, UVR_MODEL_RELEASE_BASE};
        match self {
            Self::MdxNet => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[
                    StemModelFile {
                        file_name: "kuielab_a_vocals.onnx",
                        expected_bytes: 29_703_204,
                        sha256: "daba83c2ee1afee9139766ad64c9b6808d6b6f092fff04bed3338be50baac721",
                    },
                    StemModelFile {
                        file_name: "kuielab_a_drums.onnx",
                        expected_bytes: 29_703_204,
                        sha256: "40f586b7091934dd6f5563f0cba8f14bad57ce88440da1098bf388ea716c2901",
                    },
                    StemModelFile {
                        file_name: "kuielab_a_bass.onnx",
                        expected_bytes: 29_703_204,
                        sha256: "0c3e77b9963185b1ea6bb46a4b8924137d9370fc1ccdefec7b1b416ef550dcaa",
                    },
                    StemModelFile {
                        file_name: "kuielab_a_other.onnx",
                        expected_bytes: 29_703_204,
                        sha256: "7b67a1dcb5f232153528c59960b4c7bf8dc736b8114de360af0e719633f53358",
                    },
                ],
                approx_bytes: 118_812_816,
                source_label: "UVR / Kuielab A",
            },
            Self::MdxNetKaraoke => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[StemModelFile {
                    file_name: "UVR_MDXNET_KARA.onnx",
                    expected_bytes: 29_704_436,
                    sha256: "e3167c87333a48548413e972a286bf40bf5694001d2853861eb1435953f02d63",
                }],
                approx_bytes: 29_704_436,
                source_label: "UVR MDX-NET Karaoke",
            },
            Self::MdxNetMain => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[StemModelFile {
                    file_name: "UVR_MDXNET_Main.onnx",
                    expected_bytes: 66_759_214,
                    sha256: "8289784cda38543ff431add4070662813311a8cccfc0112ca82f76d9dba2b4ca",
                }],
                approx_bytes: 66_759_214,
                source_label: "UVR MDX-NET Main",
            },
            Self::MdxNetVocFt => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[StemModelFile {
                    file_name: "UVR-MDX-NET-Voc_FT.onnx",
                    expected_bytes: 66_762_490,
                    sha256: "534b2070fcc7df514b13ef660dc8cbb328679c2374d04354a5c42bb14ecce111",
                }],
                approx_bytes: 66_762_490,
                source_label: "UVR MDX-NET Voc FT",
            },
            Self::MdxNetInstHq => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[StemModelFile {
                    file_name: "UVR-MDX-NET-Inst_HQ_3.onnx",
                    expected_bytes: 66_759_214,
                    sha256: "317554b07fe1ea5279a77f2b1520a41ea4b93432560c4ffd08792c30fddf9adc",
                }],
                approx_bytes: 66_759_214,
                source_label: "UVR MDX-NET Inst HQ 3",
            },
            Self::KimVocal => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[StemModelFile {
                    file_name: "Kim_Vocal_2.onnx",
                    expected_bytes: 66_759_214,
                    sha256: "ce74ef3b6a6024ce44211a07be9cf8bc6d87728cc852a68ab34eb8e58cde9c8b",
                }],
                approx_bytes: 66_759_214,
                source_label: "Kim Vocal 2",
            },
            Self::KimInst => StemModelPackage {
                model: self,
                base_url: UVR_MODEL_RELEASE_BASE,
                files: &[StemModelFile {
                    file_name: "Kim_Inst.onnx",
                    expected_bytes: 66_759_214,
                    sha256: "86b1940e7122fbdd2beadc65507cbff6c352d79012a8a7e60d56db98532af5f7",
                }],
                approx_bytes: 66_759_214,
                source_label: "Kim Instrumental",
            },
            Self::HtDemucs => StemModelPackage {
                model: self,
                base_url: HTDEMUCS_MODEL_BASE,
                files: &[StemModelFile {
                    file_name: "htdemucs.onnx",
                    expected_bytes: 316_446_953,
                    sha256: "68d0bf16428ef66e692cdff8a9ccf28f1ef3f69440d57e58605a4cc55fcc5e74",
                }],
                approx_bytes: 316_446_953,
                source_label: "StemSplit HT-Demucs (Hugging Face)",
            },
        }
    }
}

/// One ONNX weight file belonging to a [`StemModelPackage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StemModelFile {
    pub file_name: &'static str,
    pub expected_bytes: u64,
    pub sha256: &'static str,
}

/// Downloadable model package metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StemModelPackage {
    pub model: StemModel,
    /// Host base URL that each file name is appended to for download.
    pub base_url: &'static str,
    pub files: &'static [StemModelFile],
    /// Approximate total download size in bytes (UI hint only).
    pub approx_bytes: u64,
    pub source_label: &'static str,
}

impl StemModelPackage {
    pub fn file_count(self) -> usize {
        self.files.len()
    }

    pub fn total_bytes(self) -> u64 {
        self.files.iter().map(|f| f.expected_bytes).sum()
    }

    pub fn approx_size_label(self) -> String {
        let mb = (self.approx_bytes as f64) / (1024.0 * 1024.0);
        if mb >= 100.0 {
            format!("~{:.0} MB", mb)
        } else {
            format!("~{:.0} MB", mb)
        }
    }
}

/// Static catalog entry for the Stem Extractor model dropdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StemModelInfo {
    pub model: StemModel,
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// Ordered model list shown in the Stem Extractor dialog.
pub const STEM_MODELS: &[StemModelInfo] = &[
    StemModelInfo {
        model: StemModel::MdxNet,
        id: "mdx-net",
        label: "MDX-NET",
        description: "4-stem Kuielab A: vocals, drums, bass, other",
    },
    StemModelInfo {
        model: StemModel::MdxNetKaraoke,
        id: "mdx-net-karaoke",
        label: "MDX-NET Karaoke",
        description: "2-stem karaoke: vocals and instrumental",
    },
    StemModelInfo {
        model: StemModel::MdxNetMain,
        id: "mdx-net-main",
        label: "MDX-NET Main",
        description: "2-stem UVR Main: vocals and instrumental",
    },
    StemModelInfo {
        model: StemModel::MdxNetVocFt,
        id: "mdx-net-voc-ft",
        label: "MDX-NET Voc FT",
        description: "2-stem vocal fine-tune: vocals and instrumental",
    },
    StemModelInfo {
        model: StemModel::MdxNetInstHq,
        id: "mdx-net-inst-hq",
        label: "MDX-NET Inst HQ",
        description: "2-stem instrumental HQ: instrumental and vocals",
    },
    StemModelInfo {
        model: StemModel::KimVocal,
        id: "kim-vocal",
        label: "Kim Vocal 2",
        description: "2-stem Kim Vocal 2: vocals and instrumental",
    },
    StemModelInfo {
        model: StemModel::KimInst,
        id: "kim-inst",
        label: "Kim Instrumental",
        description: "2-stem Kim Instrumental: instrumental and vocals",
    },
    StemModelInfo {
        model: StemModel::HtDemucs,
        id: "htdemucs",
        label: "HT-Demucs",
        description: "4-stem HT-Demucs: drums, bass, other, vocals",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdx_net_is_default_and_round_trips() {
        assert_eq!(StemModel::default(), StemModel::MdxNet);
        assert_eq!(StemModel::parse("mdx-net"), Some(StemModel::MdxNet));
        assert_eq!(StemModel::MdxNet.as_str(), "mdx-net");
        assert!(StemModel::MdxNet.supports_stem(StemKind::Vocals));
        assert!(!StemModel::MdxNet.supports_stem(StemKind::Instrumental));
        assert!(StemModel::MdxNetKaraoke.supports_stem(StemKind::Instrumental));
        assert_eq!(STEM_MODELS.len(), 8);
        assert_eq!(StemModel::MdxNet.package().file_count(), 4);
        assert_eq!(StemModel::KimVocal.package().file_count(), 1);
    }

    #[test]
    fn htdemucs_is_four_stem_from_hugging_face() {
        assert_eq!(StemModel::parse("htdemucs"), Some(StemModel::HtDemucs));
        assert_eq!(StemModel::HtDemucs.as_str(), "htdemucs");
        assert_eq!(StemModel::HtDemucs.default_stems().len(), 4);
        assert!(StemModel::HtDemucs.supports_stem(StemKind::Drums));
        assert!(!StemModel::HtDemucs.supports_stem(StemKind::Instrumental));
        let package = StemModel::HtDemucs.package();
        assert_eq!(package.file_count(), 1);
        assert!(package.base_url.contains("huggingface.co"));
        assert!(
            package
                .base_url
                .contains("d54ed9eb60e258ea82131c6ee14578628816456a")
        );
        assert_eq!(package.files[0].expected_bytes, 316_446_953);
        assert_eq!(
            package.files[0].sha256,
            "68d0bf16428ef66e692cdff8a9ccf28f1ef3f69440d57e58605a4cc55fcc5e74"
        );
    }

    #[test]
    fn all_models_have_pinned_hashes_and_sizes() {
        for info in STEM_MODELS {
            let pkg = info.model.package();
            assert!(!pkg.files.is_empty());
            for file in pkg.files {
                assert!(file.expected_bytes > 0);
                assert_eq!(file.sha256.len(), 64);
                assert!(file.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            }
        }
    }
}
