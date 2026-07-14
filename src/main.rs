//! The first-party tap binary — every kindred-plugin-* crate served over
//! the tap harness (`kindred-plugins --plugin <name>`, one RON request on
//! stdin, one RON response on stdout). This is the same machinery any
//! third-party tap uses: first-party plugins are not special (ADR 0005).

use kindred_core::plugin::SourcePlugin;

fn plugin_table(name: &str) -> Option<Box<dyn SourcePlugin>> {
    match name {
        "sweep" => Some(Box::new(kindred_plugin_sweep::SweepPlugin)),
        "git-repo" => Some(Box::new(kindred_plugin_git::GitRepoPlugin)),
        "salesforce" => Some(Box::new(kindred_plugin_salesforce::SalesforcePlugin)),
        "kb" => Some(Box::new(kindred_plugin_kb::KbPlugin)),
        "graph-mail" => Some(Box::new(kindred_plugin_graph::GraphMailPlugin)),
        "graph-calendar" => Some(Box::new(kindred_plugin_graph::GraphCalendarPlugin)),
        "graph-meetings" => Some(Box::new(kindred_plugin_graph::GraphMeetingsPlugin)),
        "graph-chats" => Some(Box::new(kindred_plugin_graph::GraphChatsPlugin)),
        "sharepoint-file" => Some(Box::new(kindred_plugin_graph::SharepointFilePlugin)),
        _ => None,
    }
}

fn main() {
    kindred_core::plugin::tap_main(plugin_table);
}

#[cfg(test)]
mod manifest_drift {
    use serde::Deserialize;

    // A tolerant local mirror of the manifest shapes (the published
    // kindred-core this crate builds against may lag the config-spec
    // fields; the ENGINE parses strictly).
    #[derive(Deserialize)]
    struct Manifest {
        #[allow(dead_code)]
        tap: u32,
        #[allow(dead_code)]
        binary: String,
        plugins: Vec<Plugin>,
    }
    #[derive(Deserialize)]
    struct Plugin {
        name: String,
        #[serde(default)]
        config: Vec<Field>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default, deny_unknown_fields)]
    struct Field {
        name: String,
        doc: String,
        ty: Ty,
        required: bool,
        example: Option<String>,
        default: Option<String>,
    }
    // Mirrors kindred_core::tap::TapConfigType (bare enum variant, not a
    // string) so the manifest parses the way the engine parses it. Path is
    // a host-read capability declaration; it quotes like Str.
    #[derive(Deserialize, Default, PartialEq)]
    enum Ty {
        #[default]
        Str,
        Int,
        Bool,
        StrList,
        Ron,
        Path,
    }

    /// THE drift guard: every plugin's declared config spec, filled with its
    /// own examples/defaults, must assemble into a config the plugin's real
    /// validate_config accepts — a spec that promises a field the code
    /// rejects (or mistypes) fails here, not in an owner's install form.
    #[test]
    fn declared_config_specs_satisfy_the_plugins() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/kindred-tap.ron"))
            .expect("kindred-tap.ron");
        let manifest: Manifest = ron::from_str(&text).expect("manifest parses");
        assert_eq!(manifest.plugins.len(), 9);
        for plugin in &manifest.plugins {
            let mut parts: Vec<String> = Vec::new();
            for f in &plugin.config {
                assert!(
                    !f.doc.is_empty(),
                    "{}#{} needs a doc line",
                    plugin.name,
                    f.name
                );
                let Some(value) = f.example.as_ref().or(f.default.as_ref()) else {
                    assert!(
                        !f.required,
                        "{}#{} is required but has no example",
                        plugin.name, f.name
                    );
                    continue;
                };
                let rendered = match f.ty {
                    Ty::Int | Ty::Bool | Ty::Ron => value.clone(),
                    Ty::StrList => format!(
                        "[{}]",
                        value
                            .split(',')
                            .map(|s| format!("{:?}", s.trim()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    // Str and Path both RON-quote the raw value.
                    Ty::Str | Ty::Path => format!("{value:?}"),
                };
                parts.push(format!("{}: {rendered}", f.name));
            }
            let config = format!("({})", parts.join(", "));
            let result = super::plugin_table(&plugin.name)
                .unwrap_or_else(|| panic!("manifest advertises unserved plugin '{}'", plugin.name))
                .validate_config(&config);
            assert!(
                result.is_ok(),
                "{}: spec-assembled config rejected: {:?}\n  config: {config}",
                plugin.name,
                result.err()
            );
        }
    }
}
