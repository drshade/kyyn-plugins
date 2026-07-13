//! The first-party tap binary — every kindred-plugin-* crate served over
//! the tap harness (`kindred-plugins --plugin <name>`, one RON request on
//! stdin, one RON response on stdout). This is the same machinery any
//! third-party tap uses: first-party plugins are not special (ADR 0005).

use kindred_core::plugin::SourcePlugin;

fn main() {
    kindred_core::plugin::tap_main(|name| -> Option<Box<dyn SourcePlugin>> {
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
    });
}
