//! Surfaces — UI as data. A Surface is a widget-tree document. The shell knows
//! how to render the vocabulary; it knows nothing about weather or notes. New
//! capability + a Surface that binds to it = a new "screen" with zero shell
//! changes. The model generates and mutates these at runtime.

use serde_json::{json, Value};

pub fn default_home() -> Value {
    json!({
        "id": "home",
        "title": "Welcome",
        "widget": { "type": "stack", "children": [
            { "type": "heading", "value": "webOS" },
            { "type": "text", "value": "One capability bus. Human and AI are peer principals. Every screen is a Surface document the OS can generate and reshape." },
            { "type": "card", "title": "Try it", "children": [
                { "type": "text", "value": "• Open Weather, Notes, or System from the Dock." },
                { "type": "text", "value": "• Switch Human → AI in the menu bar, then Save a note (denied) — read still works." },
                { "type": "text", "value": "• Press ⌘Space or tap + and ask the OS to build a screen." }
            ]}
        ]}
    })
}

/// Wraps the weather.get capability. Button triggers the call (pull binding);
/// the result's `summary` field lands in the valuebox.
fn weather_card() -> Value {
    json!({ "type": "card", "title": "Weather — wraps the open-meteo API", "children": [
        { "type": "button", "label": "Refresh", "capability": "weather.get",
          "args": { "lat": 40.7128, "lon": -74.0060 }, "bindResultTo": "wx", "field": "summary" },
        { "type": "valuebox", "id": "wx" }
    ]})
}

/// Wraps local fs. As AI, Save is policy-denied while Load still works — the
/// "read but not write" demo, enforced at the bus, not in this markup.
fn notes_card() -> Value {
    json!({ "type": "card", "title": "Note — local fs (AI: write denied, read allowed)", "children": [
        { "type": "input", "id": "noteText", "placeholder": "type a note…" },
        { "type": "row", "children": [
            { "type": "button", "label": "Save", "capability": "fs.write",
              "args": { "path": "note.txt", "content": { "$input": "noteText" } },
              "bindResultTo": "noteOut", "field": "summary" },
            { "type": "button", "label": "Load", "capability": "fs.read",
              "args": { "path": "note.txt" }, "bindResultTo": "noteOut", "field": "content" }
        ]},
        { "type": "valuebox", "id": "noteOut" }
    ]})
}

pub fn weather_surface() -> Value {
    json!({ "id": "weather", "title": "Weather", "widget":
        { "type": "stack", "children": [ { "type": "heading", "value": "Weather" }, weather_card() ] } })
}

pub fn notes_surface() -> Value {
    json!({ "id": "notes", "title": "Notes", "widget":
        { "type": "stack", "children": [ { "type": "heading", "value": "Notes" }, notes_card() ] } })
}

