use std::path::{Path, PathBuf};

use airec_core::{AirecError, ErrorCode};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub defaults: DefaultsConfig,
    pub effects: EffectsConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    pub fps: Option<u32>,
    pub quality: Option<String>,
    pub cursor: Option<bool>,
    pub effects: Option<bool>,
    pub max_duration: Option<String>,
    pub on_failure: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsConfig {
    pub click_color_left: Option<String>,
    pub click_color_right: Option<String>,
    pub click_size: Option<f32>,
    pub click_duration_ms: Option<u64>,
    pub drag_color: Option<String>,
    pub drag_size: Option<f32>,
    pub drag_duration_ms: Option<u64>,
    pub trail_color: Option<String>,
    pub trail_size: Option<f32>,
    pub trail_duration_ms: Option<u64>,
    pub drag_threshold_px: Option<u32>,
    pub drag_threshold_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct LoadedConfig {
    pub path: Option<PathBuf>,
    pub value: AppConfig,
}

pub fn load() -> Result<LoadedConfig, AirecError> {
    let current = std::env::current_dir().map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            format!("cannot determine current directory: {error}"),
            serde_json::json!({"component": "config"}),
        )
    })?;
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    load_from(&current, home.as_deref())
}

fn load_from(current: &Path, home: Option<&Path>) -> Result<LoadedConfig, AirecError> {
    let current_path = current.join("airec.toml");
    let selected = if current_path.is_file() {
        Some(current_path)
    } else {
        home.map(|path| path.join("airec.toml"))
            .filter(|path| path.is_file())
    };
    let Some(path) = selected else {
        return Ok(LoadedConfig::default());
    };
    let contents = std::fs::read_to_string(&path).map_err(|error| {
        AirecError::new(
            ErrorCode::OutputIoError,
            format!("cannot read config {}: {error}", path.display()),
            serde_json::json!({"component": "config", "path": path}),
        )
    })?;
    let value = toml::from_str(&contents).map_err(|error| {
        AirecError::new(
            ErrorCode::CaptureInitFailed,
            format!("invalid config {}: {error}", path.display()),
            serde_json::json!({"component": "config", "path": path}),
        )
    })?;
    Ok(LoadedConfig {
        path: Some(path),
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirs {
        root: PathBuf,
        current: PathBuf,
        home: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("airec-config-{}", uuid::Uuid::new_v4()));
            let current = root.join("current");
            let home = root.join("home");
            std::fs::create_dir_all(&current).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            Self {
                root,
                current,
                home,
            }
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn current_directory_config_wins_over_home() {
        let dirs = TestDirs::new();
        std::fs::write(dirs.current.join("airec.toml"), "[defaults]\nfps = 24\n").unwrap();
        std::fs::write(dirs.home.join("airec.toml"), "[defaults]\nfps = 12\n").unwrap();
        let loaded = load_from(&dirs.current, Some(&dirs.home)).unwrap();
        assert_eq!(loaded.value.defaults.fps, Some(24));
        assert_eq!(loaded.path, Some(dirs.current.join("airec.toml")));
    }

    #[test]
    fn home_config_is_used_when_current_is_absent() {
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.home.join("airec.toml"),
            "[effects]\ntrail_size = 7.5\n",
        )
        .unwrap();
        let loaded = load_from(&dirs.current, Some(&dirs.home)).unwrap();
        assert_eq!(loaded.value.effects.trail_size, Some(7.5));
    }

    #[test]
    fn invalid_current_config_does_not_fall_back_to_home() {
        let dirs = TestDirs::new();
        std::fs::write(dirs.current.join("airec.toml"), "not valid toml = [").unwrap();
        std::fs::write(dirs.home.join("airec.toml"), "[defaults]\nfps = 12\n").unwrap();
        let error = load_from(&dirs.current, Some(&dirs.home)).unwrap_err();
        assert_eq!(error.code, ErrorCode::CaptureInitFailed);
        assert_eq!(error.data["component"], "config");
    }

    #[test]
    fn absent_config_uses_empty_overrides() {
        let dirs = TestDirs::new();
        let loaded = load_from(&dirs.current, Some(&dirs.home)).unwrap();
        assert!(loaded.path.is_none());
        assert!(loaded.value.defaults.fps.is_none());
    }
}
