#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FsrPreset {
    Quality,
    Balanced,
    Performance,
    /// Ratio 3.0 (FSR ultra performance, DLSS ultra performance); `VICTORY_DLSS=ultra`.
    Ultra,
}

impl FsrPreset {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "quality" => Some(Self::Quality),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
            Self::Ultra => "ultra",
        }
    }

    // AMD SDK FFX_UPSCALE_QUALITY_MODE values and ratios.
    pub(super) fn mode(self) -> u8 {
        match self {
            Self::Quality => 1,
            Self::Balanced => 2,
            Self::Performance => 3,
            Self::Ultra => 4,
        }
    }

    pub(super) fn render_scale(self) -> f32 {
        match self {
            Self::Quality => 2.0 / 3.0,
            Self::Balanced => 1.0 / 1.7,
            Self::Performance => 0.5,
            Self::Ultra => 1.0 / 3.0,
        }
    }

    pub(super) fn render_size(self) -> [u32; 2] {
        [3840.0, 2160.0].map(|size| (size * self.render_scale()) as u32)
    }
}

/// `VICTORY_DLSS=quality|balanced|performance|ultra` as the scene scale it
/// wants of the title (the ratios DLSS uses: 1.5, 1.72, 2, 3); `None` for
/// anything else (unset, empty, 0, off).
#[allow(dead_code)] // the launcher includes this file too, without the run path
pub(super) fn dlss_preset(name: &str) -> Option<FsrPreset> {
    match name.trim().to_ascii_lowercase().as_str() {
        "quality" | "1" => Some(FsrPreset::Quality),
        "balanced" => Some(FsrPreset::Balanced),
        "performance" | "perf" => Some(FsrPreset::Performance),
        "ultra" | "ultra_performance" | "ultraperformance" => Some(FsrPreset::Ultra),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dlss_modes_pick_scene_scales() {
        assert_eq!(dlss_preset("Quality"), Some(FsrPreset::Quality));
        assert_eq!(dlss_preset("ultra"), Some(FsrPreset::Ultra));
        assert_eq!(dlss_preset("off"), None);
        assert_eq!(FsrPreset::Ultra.render_size(), [1280, 720]);
        assert_eq!(FsrPreset::from_name("ultra").map(FsrPreset::mode), Some(4));
    }
}

