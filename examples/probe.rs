//! Explicit one-shot real-source check. Never run automatically in unit tests.
use ai_tool_dashboard::{adapters, model::Reading};

fn main() {
    for mut adapter in adapters::registered() {
        let snapshot = adapter.probe();
        println!("{}: {}", snapshot.name, snapshot.quota_note);
        match snapshot.activity {
            Reading::Observed { value, .. } => println!(
                "  running={}, lower_bound={}, waiting={:?}",
                value.running, value.lower_bound, value.waiting
            ),
            Reading::Unknown => println!("  activity unknown"),
            Reading::Failed => println!("  activity read failed"),
            Reading::Unsupported => println!("  activity unsupported"),
        }
        match snapshot.quota {
            Reading::Observed { value, .. } => {
                for window in value {
                    println!(
                        "  window_minutes={:?}, used_percent={:?}, reset={:?}",
                        window.window_minutes, window.used_percent, window.resets_at
                    );
                }
            }
            Reading::Unknown => println!("  unknown"),
            Reading::Unsupported => println!("  unsupported"),
            Reading::Failed => println!("  read failed (sensitive details suppressed)"),
        }
    }
}
