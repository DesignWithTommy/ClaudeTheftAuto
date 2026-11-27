//! `M1LK_*` settings: the user-facing names of the environment switches.
//!
//! The code still reads the older `VICTORY_*` names in many places. At the
//! start of every binary, [`apply`] copies each `M1LK_<NAME>` variable to
//! `VICTORY_<NAME>`, so both spellings work. If both are set, `M1LK_` wins.
//! Child processes inherit the copied values, so each process only needs to
//! call [`apply`] early in `main()`.

const NEW_PREFIX: &str = "M1LK_";
const OLD_PREFIX: &str = "VICTORY_";

/// Pure mapping: for every `M1LK_<NAME>` pair, the `(VICTORY_<NAME>, value)`
/// pair to set. Other keys are ignored.
pub fn alias_pairs<I, K, V>(vars: I) -> Vec<(String, V)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Clone,
{
    vars.into_iter()
        .filter_map(|(key, value)| {
            let name = key.as_ref().strip_prefix(NEW_PREFIX)?;
            (!name.is_empty()).then(|| (format!("{OLD_PREFIX}{name}"), value))
        })
        .collect()
}

/// Copy every `M1LK_<NAME>` environment variable to `VICTORY_<NAME>`.
///
/// Call this first in `main()`, before any thread is started.
pub fn apply() {
    let vars: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
        .collect();
    for (key, value) in alias_pairs(vars) {
        // SAFETY: `set_var` is unsound only if another thread reads or writes
        // the environment at the same time. This runs at the very start of
        // `main()`, while the process is still single-threaded. (Edition 2024
        // makes `set_var` `unsafe` for exactly this reason.)
        unsafe { std::env::set_var(key, value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_new_prefix_only() {
        let out = alias_pairs([
            ("M1LK_KEYS", "a"),
            ("VICTORY_OTHER", "b"),
            ("PATH", "c"),
            ("M1LK_", "d"),
            ("M1LK_DEBUG_HUD", "1"),
        ]);
        assert_eq!(
            out,
            vec![
                ("VICTORY_KEYS".to_string(), "a"),
                ("VICTORY_DEBUG_HUD".to_string(), "1"),
            ]
        );
    }

    #[test]
    fn m1lk_wins_over_victory() {
        // Applying the pairs after the existing variables overwrites them.
        let mut env = std::collections::HashMap::new();
        env.insert("VICTORY_X".to_string(), "old");
        for (k, v) in alias_pairs([("M1LK_X", "new")]) {
            env.insert(k, v);
        }
        assert_eq!(env["VICTORY_X"], "new");
    }
}
