//! Export-then-import an opencode session into the current working directory.
//!
//! opencode pins sessions to the directory they were created in (server-side
//! workspace routing), so to resume a session *in another project* we copy it
//! there using opencode's CLI as the API:
//!
//! 1. `opencode export <id>` — dump the session as JSON (works from any cwd)
//! 2. rewrite all IDs in the JSON so the import creates a *new* session
//!    (importing unmodified JSON would *move* the original session, because
//!    import upserts on `session.id`)
//! 3. `opencode import <file>` — runs in our cwd and pins the new session to
//!    the current directory's project
//!
//! ID format replicated from opencode's `packages/core/src/util/identifier.ts`:
//! `<prefix>_` + 12 lowercase hex chars (low 48 bits of `unix_ms * 4096 +
//! per-ms counter`, bitwise-NOT'ed for descending IDs) + 14 random base62
//! chars. Sessions use descending IDs, messages and parts ascending.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const MASK_48: u64 = 0xFFFF_FFFF_FFFF;

/// Monotonic state shared by all ID generation: (last timestamp ms, counter).
static ID_STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

fn generate_id(prefix: &str, descending: bool) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let (ms, counter) = {
        let mut state = ID_STATE.lock().expect("id state poisoned");
        if ms != state.0 {
            state.0 = ms;
            state.1 = 0;
        }
        state.1 += 1;
        (state.0, state.1)
    };
    encode_id(prefix, descending, ms, counter)
}

/// Deterministic core of ID generation, split out for tests.
fn encode_id(prefix: &str, descending: bool, ms: u64, counter: u64) -> String {
    let value = ms.wrapping_mul(4096).wrapping_add(counter);
    let value = if descending { !value } else { value } & MASK_48;
    let random: String = (0..14)
        .map(|_| BASE62[(fastrand::u8(..) % 62) as usize] as char)
        .collect();
    format!("{prefix}_{value:012x}{random}")
}

/// Rewrite an `opencode export` JSON document in place so importing it
/// creates a new session rather than moving the original.
///
/// Uses the same remap rules opencode applies when duplicating a session:
/// fresh session/message/part IDs, assistant `parentID` and compaction
/// `tail_start_id` remapped through the old->new message ID map,
/// share/revert dropped. Returns the new session ID.
pub fn rewrite_export(export: &mut Value) -> Result<String, String> {
    let new_session_id = generate_id("ses", true);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let info = export
        .get_mut("info")
        .and_then(Value::as_object_mut)
        .ok_or("export JSON has no session info")?;
    info.insert("id".into(), json!(new_session_id));
    if let Some(title) = info.get("title").and_then(Value::as_str) {
        let title = format!("{title} (imported)");
        info.insert("title".into(), json!(title));
    }
    info.insert("time".into(), json!({ "created": now, "updated": now }));
    // The copy is a fresh root session: not shared, not revertable, not bound
    // to the source session's parent or workspace (a stale workspaceID would
    // route requests back to the source project).
    info.remove("share");
    info.remove("revert");
    info.remove("parentID");
    info.remove("workspaceID");

    let messages = export
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("export JSON has no messages")?;
    let mut id_map: HashMap<String, String> = HashMap::new();

    for message in messages {
        let info = message
            .get_mut("info")
            .and_then(Value::as_object_mut)
            .ok_or("message has no info")?;
        let old_id = info
            .get("id")
            .and_then(Value::as_str)
            .ok_or("message has no id")?
            .to_string();
        let new_id = generate_id("msg", false);
        id_map.insert(old_id, new_id.clone());

        info.insert("id".into(), json!(new_id));
        info.insert("sessionID".into(), json!(new_session_id));
        if let Some(parent) = info.get("parentID").and_then(Value::as_str) {
            match id_map.get(parent) {
                Some(mapped) => {
                    let mapped = mapped.clone();
                    info.insert("parentID".into(), json!(mapped));
                }
                None => {
                    info.remove("parentID");
                }
            }
        }

        let Some(parts) = message.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts {
            let Some(part) = part.as_object_mut() else {
                continue;
            };
            part.insert("id".into(), json!(generate_id("prt", false)));
            part.insert("sessionID".into(), json!(new_session_id));
            part.insert("messageID".into(), json!(new_id));
            if let Some(tail) = part.get("tail_start_id").and_then(Value::as_str) {
                match id_map.get(tail) {
                    Some(mapped) => {
                        let mapped = mapped.clone();
                        part.insert("tail_start_id".into(), json!(mapped));
                    }
                    None => {
                        part.remove("tail_start_id");
                    }
                }
            }
        }
    }

    Ok(new_session_id)
}

/// Copy `session_id` into the current working directory via
/// `opencode export` + `opencode import`. Returns the new session ID.
pub fn import_session(session_id: &str) -> Result<String, String> {
    let exported = Command::new("opencode")
        .arg("export")
        .arg(session_id)
        .output()
        .map_err(|e| format!("failed to run `opencode export`: {e}"))?;
    if !exported.status.success() {
        return Err(format!(
            "`opencode export {session_id}` failed: {}",
            String::from_utf8_lossy(&exported.stderr).trim()
        ));
    }

    let mut export: Value = serde_json::from_slice(&exported.stdout)
        .map_err(|e| format!("could not parse `opencode export` output: {e}"))?;
    let new_session_id = rewrite_export(&mut export)?;

    let tmp = std::env::temp_dir().join(format!("opencode-import-{new_session_id}.json"));
    std::fs::write(
        &tmp,
        serde_json::to_vec(&export).expect("serialize rewritten export"),
    )
    .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;

    let imported = Command::new("opencode").arg("import").arg(&tmp).output();
    let _ = std::fs::remove_file(&tmp);
    let imported = imported.map_err(|e| format!("failed to run `opencode import`: {e}"))?;
    if !imported.status.success() {
        return Err(format!(
            "`opencode import` failed: {}",
            String::from_utf8_lossy(&imported.stderr).trim()
        ));
    }

    Ok(new_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_format() {
        let id = generate_id("ses", true);
        assert!(id.starts_with("ses_"));
        assert_eq!(id.len(), "ses_".len() + 26);
        let body = &id["ses_".len()..];
        assert!(
            body[..12]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert!(body[12..].bytes().all(|b| BASE62.contains(&b)));
    }

    #[test]
    fn id_encoding_matches_opencode() {
        // Expected values computed with opencode's identifier.ts:
        // Identifier.create(false, 1718000000000) with counter=1 and
        // Identifier.create(true, 1718000000000) with counter=1.
        let asc = encode_id("msg", false, 1718000000000, 1);
        assert_eq!(&asc[4..16], "000c79c00001");
        let desc = encode_id("ses", true, 1718000000000, 1);
        assert_eq!(&desc[4..16], "fff3863ffffe");
    }

    #[test]
    fn ascending_ids_sort_in_generation_order() {
        let a = generate_id("msg", false);
        let b = generate_id("msg", false);
        assert!(a[4..16] < b[4..16], "{a} should sort before {b}");
    }

    #[test]
    fn descending_ids_sort_in_reverse_generation_order() {
        let a = generate_id("ses", true);
        let b = generate_id("ses", true);
        assert!(a[4..16] > b[4..16], "{a} should sort after {b}");
    }

    #[test]
    fn rewrite_remaps_all_ids() {
        let mut export = json!({
            "info": {
                "id": "ses_old",
                "title": "my session",
                "slug": "brave-otter",
                "parentID": "ses_parent",
                "workspaceID": "wsp_old",
                "share": { "url": "https://example.com" },
                "revert": { "messageID": "msg_a" },
                "time": { "created": 1, "updated": 2 }
            },
            "messages": [
                {
                    "info": { "id": "msg_a", "sessionID": "ses_old", "role": "user" },
                    "parts": [
                        { "id": "prt_a1", "sessionID": "ses_old", "messageID": "msg_a", "type": "text", "text": "hi" }
                    ]
                },
                {
                    "info": { "id": "msg_b", "sessionID": "ses_old", "role": "assistant", "parentID": "msg_a" },
                    "parts": [
                        { "id": "prt_b1", "sessionID": "ses_old", "messageID": "msg_b", "type": "compaction", "tail_start_id": "msg_a" },
                        { "id": "prt_b2", "sessionID": "ses_old", "messageID": "msg_b", "type": "compaction", "tail_start_id": "msg_missing" }
                    ]
                }
            ]
        });

        let new_id = rewrite_export(&mut export).expect("rewrite");
        assert!(new_id.starts_with("ses_"));

        let info = &export["info"];
        assert_eq!(info["id"], json!(new_id));
        assert_eq!(info["title"], json!("my session (imported)"));
        assert_eq!(info["slug"], json!("brave-otter"));
        assert!(info.get("share").is_none());
        assert!(info.get("revert").is_none());
        assert!(info.get("parentID").is_none());
        assert!(info.get("workspaceID").is_none());

        let messages = export["messages"].as_array().unwrap();
        let msg_a = &messages[0]["info"];
        let msg_b = &messages[1]["info"];
        let new_a = msg_a["id"].as_str().unwrap();
        let new_b = msg_b["id"].as_str().unwrap();
        assert!(new_a.starts_with("msg_") && new_a != "msg_a");
        assert!(new_b.starts_with("msg_") && new_b != "msg_b");
        assert_eq!(msg_a["sessionID"], json!(new_id));
        // assistant parentID remapped to the user message's new ID
        assert_eq!(msg_b["parentID"], json!(new_a));

        let part_a = &messages[0]["parts"][0];
        assert!(part_a["id"].as_str().unwrap().starts_with("prt_"));
        assert_ne!(part_a["id"], json!("prt_a1"));
        assert_eq!(part_a["sessionID"], json!(new_id));
        assert_eq!(part_a["messageID"], json!(new_a));
        assert_eq!(part_a["text"], json!("hi"));

        // compaction tail remapped; unknown tail dropped
        assert_eq!(messages[1]["parts"][0]["tail_start_id"], json!(new_a));
        assert!(messages[1]["parts"][1].get("tail_start_id").is_none());
        assert_eq!(messages[1]["parts"][0]["messageID"], json!(new_b));
    }

    /// End-to-end test against the real `opencode` CLI, sandboxed via
    /// XDG_* env vars so the user's data is untouched. Requires `opencode`
    /// on PATH; mutates process env and cwd, so it is ignored by default.
    /// Run alone with: cargo test e2e_import -- --ignored
    #[test]
    #[ignore]
    fn e2e_import_into_other_git_dir() {
        let root = std::env::temp_dir().join(format!("opencode-import-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for sub in ["xdg/data", "xdg/config", "xdg/state", "xdg/cache", "a", "b"] {
            std::fs::create_dir_all(root.join(sub)).expect("mkdir");
        }
        unsafe {
            std::env::set_var("XDG_DATA_HOME", root.join("xdg/data"));
            std::env::set_var("XDG_CONFIG_HOME", root.join("xdg/config"));
            std::env::set_var("XDG_STATE_HOME", root.join("xdg/state"));
            std::env::set_var("XDG_CACHE_HOME", root.join("xdg/cache"));
        }
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        for dir in [&dir_a, &dir_b] {
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir)
                .status()
                .expect("git init");
            assert!(status.success());
            // A root commit gives each repo a distinct opencode project ID
            // (repos without remote/commits all fall back to "global").
            let status = Command::new("git")
                .args([
                    "-c",
                    "user.email=e2e@example.com",
                    "-c",
                    "user.name=e2e",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    &format!("init {}", dir.display()),
                ])
                .current_dir(dir)
                .status()
                .expect("git commit");
            assert!(status.success());
        }

        // Fabricate a minimal source session and import it from dir A.
        let src_id = generate_id("ses", true);
        let src_msg = generate_id("msg", false);
        let source = json!({
            "info": {
                "id": src_id,
                "slug": "brave-otter",
                "title": "e2e source",
                "version": "0.0.0",
                "time": { "created": 1, "updated": 2 }
            },
            "messages": [
                {
                    "info": {
                        "id": src_msg,
                        "sessionID": src_id,
                        "role": "user",
                        "time": { "created": 3 },
                        "agent": "build",
                        "model": { "providerID": "test", "modelID": "test-model" }
                    },
                    "parts": [
                        {
                            "id": generate_id("prt", false),
                            "sessionID": src_id,
                            "messageID": src_msg,
                            "type": "text",
                            "text": "hello from a"
                        }
                    ]
                }
            ]
        });
        let source_path = root.join("source.json");
        std::fs::write(&source_path, serde_json::to_vec(&source).unwrap()).expect("write source");
        let imported = Command::new("opencode")
            .arg("import")
            .arg(&source_path)
            .current_dir(&dir_a)
            .output()
            .expect("run opencode import");
        assert!(
            imported.status.success(),
            "source import failed: {}",
            String::from_utf8_lossy(&imported.stderr)
        );

        // Export-then-import from dir B.
        std::env::set_current_dir(&dir_b).expect("chdir b");
        let new_id = import_session(&src_id).expect("import_session");
        assert_ne!(new_id, src_id);

        // Inspect the sandbox DB.
        let db_path = std::fs::read_dir(root.join("xdg/data/opencode"))
            .expect("data dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "db"))
            .expect("sandbox db");
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open db");

        // From dir B: the copy matches the cwd project, the source does not.
        assert!(crate::same_project(Some(&db_path), &new_id));
        assert!(!crate::same_project(Some(&db_path), &src_id));
        // From dir A the source matches, so Enter would plain-resume it.
        std::env::set_current_dir(&dir_a).expect("chdir a");
        assert!(crate::same_project(Some(&db_path), &src_id));
        std::env::set_current_dir(&dir_b).expect("chdir b");

        let (copy_dir, copy_project, copy_title): (String, String, String) = conn
            .query_row(
                "SELECT directory, project_id, title FROM session WHERE id = ?1",
                [&new_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("imported session row");
        assert_eq!(copy_dir, dir_b.to_string_lossy());
        assert_eq!(copy_title, "e2e source (imported)");

        // Original session untouched, still pinned to dir A in its own project.
        let (src_dir, src_project): (String, String) = conn
            .query_row(
                "SELECT directory, project_id FROM session WHERE id = ?1",
                [&src_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("source session row");
        assert_eq!(src_dir, dir_a.to_string_lossy());
        assert_ne!(copy_project, src_project);

        // Messages and parts copied under fresh IDs.
        let copy_msgs: i64 = conn
            .query_row(
                "SELECT count(*) FROM message WHERE session_id = ?1",
                [&new_id],
                |row| row.get(0),
            )
            .unwrap();
        let copy_parts: i64 = conn
            .query_row(
                "SELECT count(*) FROM part WHERE session_id = ?1",
                [&new_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((copy_msgs, copy_parts), (1, 1));

        let _ = std::fs::remove_dir_all(&root);
    }
}
