//! `sch_bus` toolset — buses, bus entries, and pin-to-bus fan-out.
//!
//! A KiCad bus is three things drawn together, and all three have to agree or
//! the bus carries nothing:
//!
//!   1. the **bus segment** itself — geometrically a wire, distinguished only
//!      by the node name and by the label attached to it;
//!   2. a **bus entry** per member — the 45° tick that bridges the gap between
//!      a wire and the bus. It is not decoration: without it the wire and the
//!      bus do not touch, because they are deliberately drawn apart;
//!   3. a **label on every wire stub**, naming the member signal, plus a label
//!      on the bus naming the group.
//!
//! Membership is by *name*, never by geometry: a stub joins the bus because its
//! label matches a member of the bus label, so `connect_pins_to_bus` writes all
//! three parts from one call rather than leaving the caller to line them up.
//!
//! Bus label syntax, all accepted by `add_schematic_net_label`:
//!   - vector  `X_DIG[1..6]`   — members `X_DIG1` … `X_DIG6`
//!   - group   `{A B C}`       — members named individually
//!   - alias   `DSI1{DSI}`     — members come from a `(bus_alias "DSI" …)`
//!     defined once anywhere in the hierarchy, expanding to `DSI1.D0_P` etc.
//!
//! The alias form is the one that fails silently. eeschema's Schematic Setup →
//! Bus Aliases dialog edits **only the sheet you have open**, and a reference
//! to an alias no sheet defines is not an error: KiCad reads `DSI1{DSI}` as a
//! one-member group and nets up `DSI1.DSI`. The schematic looks right, the
//! netlist is wrong. `validate_bus_aliases` is the check for exactly that.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_f64, opt_str, require_f64, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    parser::parse_sexp,
    schematic::{
        escape_quoted_text, extract_lib_pins_for_unit, extract_symbol_instances, format_bus,
        format_bus_entry, format_net_label, format_wire, pin_endpoint, BusEntryDirection,
    },
    writer::write_atomic,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_bus",
            "Add a bus segment between two points. Geometrically a wire, but KiCad treats it as a \
             bus: it carries the members named by the bus label attached to it (vector \
             'NAME[1..6]' or group '{A B C}'), and ordinary wires join it only through a bus entry.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "x1": { "type": "number" },
                    "y1": { "type": "number" },
                    "x2": { "type": "number" },
                    "y2": { "type": "number" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_bus(args, ctx).await }
        ),
        tool!(
            "batch_add_bus",
            "Add multiple bus segments in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "buses": {
                        "type": "array",
                        "description": "List of {x1,y1,x2,y2} segments",
                        "items": {
                            "type": "object",
                            "properties": {
                                "x1": { "type": "number" }, "y1": { "type": "number" },
                                "x2": { "type": "number" }, "y2": { "type": "number" }
                            },
                            "required": ["x1", "y1", "x2", "y2"]
                        }
                    }
                },
                "required": ["schematic", "buses"]
            }),
            |args, ctx| async move { handle_batch_add_bus(args, ctx).await }
        ),
        tool!(
            "add_bus_entry",
            "Add a bus entry — the 45-degree tick that connects a wire to a bus. Required: a wire \
             and a bus that merely touch are NOT connected without one. 'x'/'y' are the end on \
             the wire side; 'direction' says which way the tick runs to reach the bus.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "x": { "type": "number", "description": "X of the wire-side end" },
                    "y": { "type": "number", "description": "Y of the wire-side end" },
                    "direction": {
                        "type": "string",
                        "description": "Which way the tick runs from 'at' to the bus",
                        "enum": ["down_right", "down_left", "up_right", "up_left"],
                        "default": "down_right"
                    }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_add_bus_entry(args, ctx).await }
        ),
        tool!(
            "connect_pins_to_bus",
            "Fan a set of pins out onto a bus: for each pin, a wire stub from the pin to the bus \
             entry, the bus entry itself, and a net label naming the member. This is the whole \
             connection — a stub without a label joins nothing, since bus membership is by name. \
             Give the bus's fixed coordinate ('bus_y' for a horizontal bus, 'bus_x' for a \
             vertical one); the bus segment itself is drawn separately with add_bus.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "bus_y": { "type": "number", "description": "Y of a horizontal bus (omit if vertical)" },
                    "bus_x": { "type": "number", "description": "X of a vertical bus (omit if horizontal)" },
                    "connections": {
                        "type": "array",
                        "description": "List of {reference, pin_number, net} — net is the bus member name",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" },
                                "net": { "type": "string" }
                            },
                            "required": ["reference", "pin_number", "net"]
                        }
                    },
                },
                "required": ["schematic", "connections"]
            }),
            |args, ctx| async move { handle_connect_pins_to_bus(args, ctx).await }
        ),
        tool!(
            "add_bus_alias",
            "Define a bus alias on a sheet: (bus_alias \"NAME\" (members ...)). An alias lets a \
             label or sheet pin name a whole bundle as 'PREFIX{NAME}', which KiCad expands into \
             one 'PREFIX.MEMBER' net per member. Written where eeschema writes it, after \
             lib_symbols. Idempotent: redefining a name replaces that block's member list rather \
             than leaving two definitions of the same alias in one file.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "name": { "type": "string", "description": "Alias name, as used inside the braces of 'PREFIX{NAME}'" },
                    "members": {
                        "type": "array",
                        "description": "Member signal names, in order. Each becomes 'PREFIX.MEMBER'.",
                        "items": { "type": "string" },
                        "minItems": 1
                    }
                },
                "required": ["schematic", "name", "members"]
            }),
            |args, ctx| async move { handle_add_bus_alias(args, ctx).await }
        ),
        tool!(
            "list_bus_aliases",
            "List the bus aliases defined in a sheet and, by default, every sheet below it. \
             Reports which file defines each alias, because eeschema's Bus Aliases dialog only \
             ever shows the sheet currently open.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "recursive": {
                        "type": "boolean",
                        "description": "Walk child sheets too (default true)",
                        "default": true
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_list_bus_aliases(args, ctx).await }
        ),
        tool!(
            "validate_bus_aliases",
            "Check every 'PREFIX{ALIAS}' label, global label, hierarchical label and sheet pin in \
             the hierarchy against the aliases actually defined in it. Reports an alias referenced \
             but defined nowhere -- which KiCad does NOT flag, it silently nets up a one-member \
             group named after the alias -- and the same alias defined in two files with different \
             members. Aliases defined but never used are listed separately, as information rather \
             than a fault. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to the ROOT .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_bus_aliases(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_bus(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (x1, y1, x2, y2) = match read_xy_pair(args) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    if (x1 - x2).abs() > 0.01 && (y1 - y2).abs() > 0.01 {
        return Ok(CallToolResult::error(
            "Bus segment must be horizontal or vertical",
        ));
    }

    let content = std::fs::read_to_string(&sch_path)?;
    let new_content =
        crate::tools::sch_wiring::insert_before_close(&content, &format_bus(x1, y1, x2, y2));
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": "bus",
        "from": { "x": x1, "y": y1 },
        "to": { "x": x2, "y": y2 }
    })))
}

async fn handle_batch_add_bus(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let items = match args["buses"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'buses' array")),
    };

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let (x1, y1, x2, y2) = match read_xy_pair(item) {
            Ok(v) => v,
            Err(_) => {
                errors.push(format!("Segment {i}: needs x1, y1, x2, y2"));
                continue;
            }
        };
        if (x1 - x2).abs() > 0.01 && (y1 - y2).abs() > 0.01 {
            errors.push(format!("Segment {i}: must be horizontal or vertical"));
            continue;
        }
        inserts.push('\n');
        inserts.push_str("  ");
        inserts.push_str(&format_bus(x1, y1, x2, y2));
        added.push(json!({ "from": {"x": x1, "y": y1}, "to": {"x": x2, "y": y2} }));
    }

    let content = std::fs::read_to_string(&sch_path)?;
    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added_count": added.len(),
        "added": added,
        "errors": errors
    })))
}

async fn handle_add_bus_entry(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let raw = opt_str(args, "direction").unwrap_or("down_right");
    let direction = match parse_direction(raw) {
        Some(d) => d,
        None => {
            return Ok(CallToolResult::error(format!(
                "Unknown direction '{raw}'. Valid: down_right, down_left, up_right, up_left"
            )))
        }
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let new_content =
        crate::tools::sch_wiring::insert_before_close(&content, &format_bus_entry(x, y, direction));
    write_atomic(&sch_path, &new_content)?;

    // Which end is the bus side is a fact about the sheet, not about the
    // request: the entry's `at` corner is wherever the caller put it. The
    // documented convention (x/y = wire side) only holds when the caller
    // follows it, and the response used to assert it unchecked — labelling
    // the on-bus corner `wire_side` for anyone who gave the bus point
    // instead (#329). Judge both corners against the sheet's actual bus
    // segments and only fall back to the convention when geometry cannot
    // decide.
    let round6 = |value: f64| (value * 1e6).round() / 1e6;
    let far = (
        round6(x + direction.size().0),
        round6(y + direction.size().1),
    );
    let (at_on_bus, far_on_bus) = match parse_sexp(&content) {
        Ok(tree) => {
            let buses = konnect_sexp::schematic::extract_buses(&tree);
            let on_bus = |p: (f64, f64)| {
                buses.iter().any(|bus| {
                    konnect_sexp::geometry::point_on_segment(
                        p.0,
                        p.1,
                        bus.x1,
                        bus.y1,
                        bus.x2,
                        bus.y2,
                        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
                    )
                })
            };
            (on_bus((x, y)), on_bus(far))
        }
        Err(_) => (false, false),
    };
    let (bus_side, wire_side, note) = match (at_on_bus, far_on_bus) {
        (true, false) => ((x, y), far, None),
        (false, true) => (far, (x, y), None),
        (true, true) => (
            far,
            (x, y),
            Some("both ends of this entry touch a bus; sides are reported from the documented convention (x/y = wire side)"),
        ),
        (false, false) => (
            far,
            (x, y),
            Some("no bus touches this entry; sides are reported from the documented convention (x/y = wire side)"),
        ),
    };
    let mut response = json!({
        "added": "bus_entry",
        "wire_side": { "x": wire_side.0, "y": wire_side.1 },
        "bus_side": { "x": bus_side.0, "y": bus_side.1 }
    });
    if let Some(note) = note {
        response["note"] = json!(note);
    }
    Ok(CallToolResult::json(&response))
}

async fn handle_connect_pins_to_bus(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let conns = match args["connections"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'connections' array")),
    };
    let bus_y = opt_f64(args, "bus_y");
    let bus_x = opt_f64(args, "bus_x");
    let size = 2.54; // BusEntryDirection fixes the tick at one grid step

    let horizontal = match (bus_y, bus_x) {
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => {
            return Ok(CallToolResult::error(
                "Give exactly one of 'bus_y' (horizontal bus) or 'bus_x' (vertical bus)",
            ))
        }
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let tree = match parse_sexp(&content) {
        Ok(t) => t,
        Err(e) => return Ok(CallToolResult::error(format!("Parse error: {e}"))),
    };
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for conn in &conns {
        let (reference, pin_number, net) = match (
            conn["reference"].as_str(),
            conn["pin_number"].as_str(),
            conn["net"].as_str(),
        ) {
            (Some(r), Some(p), Some(n)) => (r, p, n),
            _ => {
                errors.push("Each connection needs reference, pin_number and net".into());
                continue;
            }
        };

        // Resolve against the unit that actually owns the pin — a multi-unit
        // part repeats its reference on every unit.
        let ep = instances
            .iter()
            .filter(|i| i.reference == reference)
            .find_map(|inst| {
                let sym = lib_syms
                    .iter()
                    .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
                extract_lib_pins_for_unit(sym, inst.unit)
                    .into_iter()
                    .find(|p| p.number == pin_number)
                    .map(|p| pin_endpoint(&p, inst.pin_transform()))
            });

        let (px, py) = match ep {
            Some(v) => v,
            None => {
                errors.push(format!("Pin {pin_number} of '{reference}' not found"));
                continue;
            }
        };

        // The entry bridges the last `size` before the bus; the stub covers the
        // rest of the way from the pin.
        let (entry_x, entry_y, dx, dy, sx, sy) = if horizontal {
            let by = bus_y.unwrap();
            let dir = if by > py { 1.0 } else { -1.0 };
            let ey = by - dir * size;
            (px, ey, dir * size, dir * size, px, py)
        } else {
            let bx = bus_x.unwrap();
            let dir = if bx > px { 1.0 } else { -1.0 };
            let ex = bx - dir * size;
            (ex, py, dir * size, dir * size, px, py)
        };

        // Stub from the pin up to the entry's wire-side end. Zero-length when
        // the pin already sits there; KiCad rejects a degenerate wire.
        if (sx - entry_x).abs() > 0.01 || (sy - entry_y).abs() > 0.01 {
            inserts.push_str("\n  ");
            inserts.push_str(&format_wire(sx, sy, entry_x, entry_y));
        }
        inserts.push_str("\n  ");
        inserts.push_str(&format_bus_entry(entry_x, entry_y, entry_dir(dx, dy)));
        // The label is what actually puts this stub on the bus.
        let rot = if horizontal { 90.0 } else { 0.0 };
        inserts.push_str("\n  ");
        inserts.push_str(&format_net_label(net, sx, sy, rot));

        added.push(json!({
            "reference": reference, "pin": pin_number, "net": net,
            "pin_at": { "x": px, "y": py },
            "entry_at": { "x": entry_x, "y": entry_y }
        }));
    }

    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "connected_count": added.len(),
        "connected": added,
        "errors": errors
    })))
}

// ─── Bus alias handlers ──────────────────────────────────────────────────────

async fn handle_add_bus_alias(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let name = match opt_str(args, "name") {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        _ => return Ok(CallToolResult::error("Missing 'name'")),
    };
    if name.chars().any(char::is_whitespace) {
        return Ok(CallToolResult::error(format!(
            "Alias name '{name}' contains whitespace. KiCad splits a group bus on whitespace, so \
             a 'PREFIX{{NAME}}' reference could never resolve to it."
        )));
    }
    let members: Vec<String> = match args["members"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => return Ok(CallToolResult::error("Missing 'members' array")),
    };
    if members.is_empty() {
        return Ok(CallToolResult::error(
            "'members' must name at least one signal",
        ));
    }
    if let Some(bad) = members
        .iter()
        .find(|m| m.trim().is_empty() || m.chars().any(char::is_whitespace))
    {
        return Ok(CallToolResult::error(format!(
            "Member '{bad}' is empty or contains whitespace; KiCad splits members on whitespace."
        )));
    }

    let content = std::fs::read_to_string(&sch_path)?;
    // Match the file's own indentation, or a targeted edit comes back as a
    // whole-file reindent the next time eeschema saves.
    let indent = if content.contains("\n\t(") { "\t" } else { "  " };
    let block = format_bus_alias(&name, &members, indent);

    let (new_content, action, previous) = match find_alias_span(&content, &name) {
        Some((start, end)) => {
            // Parse the whole sheet, not the block: find_all searches a node's
            // children, so a tree whose root IS the bus_alias matches nothing.
            let previous = parse_bus_aliases(&content)
                .into_iter()
                .find(|a| a.name == name)
                .map(|a| a.members);
            (
                format!("{}{}{}", &content[..start], block, &content[end..]),
                "replaced",
                previous,
            )
        }
        None => {
            let at = match alias_insert_offset(&content) {
                Some(offset) => offset,
                None => {
                    return Ok(CallToolResult::error(
                        "Could not find anywhere to write the alias: the sheet has neither a \
                         lib_symbols block nor a (uuid ...) node to anchor against.",
                    ))
                }
            };
            (
                format!("{}\n{indent}{}{}", &content[..at], block, &content[at..]),
                "added",
                None,
            )
        }
    };
    write_atomic(&sch_path, &new_content)?;

    let mut response = json!({
        "action": action,
        "name": name,
        "members": members,
        "member_count": members.len(),
        "reference_form": format!("PREFIX{{{name}}}"),
        "schematic": sch_path.display().to_string()
    });
    if let Some(previous) = previous {
        response["previous_members"] = json!(previous);
    }
    Ok(CallToolResult::json(&response))
}

async fn handle_list_bus_aliases(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root = get_path(args, "schematic")?;
    if !root.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root.display()
        )));
    }
    let recursive = args["recursive"].as_bool().unwrap_or(true);
    let files = if recursive {
        hierarchy_files(&root)
    } else {
        vec![root.clone()]
    };

    let definitions = collect_alias_definitions(&files);
    let mut by_name: BTreeMap<String, Vec<AliasDefinition>> = BTreeMap::new();
    for definition in definitions {
        by_name
            .entry(definition.name.clone())
            .or_default()
            .push(definition);
    }

    let aliases: Vec<serde_json::Value> = by_name
        .iter()
        .map(|(name, defs)| {
            let distinct: BTreeSet<&Vec<String>> = defs.iter().map(|d| &d.members).collect();
            json!({
                "name": name,
                "members": defs[0].members,
                "defined_in": defs.iter().map(|d| d.file.clone()).collect::<Vec<_>>(),
                "consistent": distinct.len() == 1
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "sheets_scanned": files.len(),
        "alias_count": aliases.len(),
        "aliases": aliases
    })))
}

async fn handle_validate_bus_aliases(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root = get_path(args, "schematic")?;
    if !root.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root.display()
        )));
    }
    let files = hierarchy_files(&root);
    let definitions = collect_alias_definitions(&files);
    let uses = collect_alias_uses(&files);

    let mut by_name: BTreeMap<String, Vec<AliasDefinition>> = BTreeMap::new();
    for definition in definitions {
        by_name
            .entry(definition.name.clone())
            .or_default()
            .push(definition);
    }

    let mut issues: Vec<serde_json::Value> = Vec::new();

    // Same alias, two files, different members. KiCad resolves this by
    // whichever definition its connectivity pass reaches first, which is not a
    // thing to leave to chance.
    for (name, defs) in &by_name {
        let distinct: BTreeSet<&Vec<String>> = defs.iter().map(|d| &d.members).collect();
        if distinct.len() > 1 {
            issues.push(json!({
                "kind": "conflicting_definition",
                "alias": name,
                "definitions": defs.iter().map(|d| json!({
                    "file": d.file, "members": d.members
                })).collect::<Vec<_>>(),
                "why": "the same alias is defined more than once with different members; which one \
                        wins is not defined by the file format"
            }));
        }
    }

    // The silent one: a reference to an alias nothing defines. KiCad does not
    // report it — it reads `PREFIX{ALIAS}` as a one-member group and nets up
    // `PREFIX.ALIAS`.
    let mut unresolved: BTreeMap<&str, Vec<&AliasUse>> = BTreeMap::new();
    for use_site in &uses {
        if !by_name.contains_key(&use_site.alias) {
            unresolved
                .entry(use_site.alias.as_str())
                .or_default()
                .push(use_site);
        }
    }
    for (alias, sites) in unresolved {
        issues.push(json!({
            "kind": "unresolved_alias",
            "alias": alias,
            "used_at": sites.iter().map(|u| json!({
                "label": u.label, "file": u.file, "item": u.kind
            })).collect::<Vec<_>>(),
            "why": format!(
                "no sheet in this hierarchy defines '{alias}'. KiCad does not flag that: it reads \
                 the label as a one-member group and nets up 'PREFIX.{alias}'. The schematic looks \
                 right and the netlist is wrong."
            )
        }));
    }

    let referenced: HashSet<&str> = uses.iter().map(|u| u.alias.as_str()).collect();
    let unused: Vec<serde_json::Value> = by_name
        .iter()
        .filter(|(name, _)| !referenced.contains(name.as_str()))
        .map(|(name, defs)| {
            json!({
                "alias": name,
                "defined_in": defs.iter().map(|d| d.file.clone()).collect::<Vec<_>>()
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "sheets_scanned": files.len(),
        "aliases_defined": by_name.len(),
        "alias_references": uses.len(),
        "issue_count": issues.len(),
        "issues": issues,
        "unused_aliases": unused
    })))
}

// ─── Bus alias helpers ───────────────────────────────────────────────────────

/// One `(bus_alias "NAME" (members ...))` block and the file it came from.
struct AliasDefinition {
    name: String,
    members: Vec<String>,
    file: String,
}

/// One `PREFIX{ALIAS}` reference and where it was written.
struct AliasUse {
    alias: String,
    label: String,
    file: String,
    kind: &'static str,
}

/// Split a group-bus label into (prefix, inner): `DSI1{DSI}` -> ("DSI1", "DSI").
///
/// Matched from the END with a depth counter, not by the first or last brace,
/// because KiCad's subscript markup puts braces inside names: the real label
/// `I_{2}C1{I_{2}C}` is prefix `I_{2}C1` and inner `I_{2}C`, and either naive
/// scan gets it wrong.
fn split_group_bus(label: &str) -> Option<(&str, &str)> {
    let bytes = label.as_bytes();
    if bytes.last() != Some(&b'}') {
        return None;
    }
    let mut depth = 0usize;
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    // `~{X}`, `_{X}`, `^{X}` are KiCad TEXT MARKUP — overbar,
                    // subscript, superscript — not a bus. Active-low names
                    // like `~{RESET}` and `~{CS_ACCEL}` are ordinary nets, and
                    // reading them as group buses invents aliases nothing
                    // defines.
                    if i > 0 && matches!(bytes[i - 1], b'~' | b'_' | b'^') {
                        return None;
                    }
                    return Some((&label[..i], &label[i + 1..bytes.len() - 1]));
                }
            }
            _ => {}
        }
    }
    None
}

/// The alias a label references, if it references one. An inner token holding
/// whitespace is an explicit member list (`{A B C}`) and names no alias.
fn alias_reference(label: &str) -> Option<&str> {
    let (_, inner) = split_group_bus(label)?;
    if inner.is_empty() || inner.chars().any(char::is_whitespace) {
        None
    } else {
        Some(inner)
    }
}

fn format_bus_alias(name: &str, members: &[String], indent: &str) -> String {
    let quoted: Vec<String> = members
        .iter()
        .map(|m| format!("\"{}\"", escape_quoted_text(m)))
        .collect();
    format!(
        "(bus_alias \"{}\"\n{indent}{indent}(members {})\n{indent})",
        escape_quoted_text(name),
        quoted.join(" ")
    )
}

/// Byte offset of the `)` closing the node that opens at `start`, honouring
/// quoted strings — symbol Description fields routinely contain parentheses,
/// and a naive depth count walks straight past the real end.
fn close_of_node(content: &str, start: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    if bytes.get(start) != Some(&b'(') {
        return None;
    }
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where to write a new alias: straight after the `lib_symbols` block, which is
/// where eeschema puts them, so its next save produces no diff of its own.
/// Falls back to after the sheet `(uuid ...)`.
fn alias_insert_offset(content: &str) -> Option<usize> {
    for opener in ["\n\t(lib_symbols", "\n  (lib_symbols", "\n\t(uuid ", "\n  (uuid "] {
        let Some(found) = content.find(opener) else {
            continue;
        };
        // The opener carries the newline and the indent, so the `(` it names is
        // further in than `found + 1` — close_of_node insists on landing on it.
        let node_start = found + opener.find('(')?;
        if opener.contains("lib_symbols") {
            return close_of_node(content, node_start).map(|end| end + 1);
        }
        // `(uuid "…")` is a single line; end it at its own newline.
        return content[node_start..].find('\n').map(|nl| node_start + nl);
    }
    None
}

/// Span of an existing `(bus_alias "NAME" …)` block, start..end exclusive.
fn find_alias_span(content: &str, name: &str) -> Option<(usize, usize)> {
    let needle = format!("(bus_alias \"{}\"", escape_quoted_text(name));
    let start = content.find(&needle)?;
    let end = close_of_node(content, start)?;
    Some((start, end + 1))
}

/// Every alias defined in one sheet's raw text.
fn parse_bus_aliases(content: &str) -> Vec<AliasDefinition> {
    let tree = match parse_sexp(content) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    tree.find_all("bus_alias")
        .iter()
        .filter_map(|node| {
            let name = node.get(1)?.as_str()?.to_string();
            let members = node
                .find("members")
                .map(|m| {
                    m.children()
                        .unwrap_or(&[])
                        .iter()
                        .skip(1)
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(AliasDefinition {
                name,
                members,
                file: String::new(),
            })
        })
        .collect()
}

/// Root sheet plus every sheet below it, de-duplicated, cycle-safe.
fn hierarchy_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk_hierarchy(root, 0, &mut visited, &mut out);
    out
}

fn walk_hierarchy(path: &Path, depth: usize, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if depth > crate::tools::sch_hierarchy::MAX_HIERARCHY_DEPTH || !visited.insert(canon) {
        return;
    }
    out.push(path.to_path_buf());
    let Ok(sch) = cse::Schematic::load(path) else {
        return;
    };
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    for sheet in sch.sheets.iter() {
        let child = dir.join(sheet.file());
        if child.exists() {
            walk_hierarchy(&child, depth + 1, visited, out);
        }
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn collect_alias_definitions(files: &[PathBuf]) -> Vec<AliasDefinition> {
    let mut out = Vec::new();
    for path in files {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for mut definition in parse_bus_aliases(&content) {
            definition.file = file_label(path);
            out.push(definition);
        }
    }
    out
}

fn collect_alias_uses(files: &[PathBuf]) -> Vec<AliasUse> {
    let mut out = Vec::new();
    for path in files {
        let Ok(sch) = cse::Schematic::load(path) else {
            continue;
        };
        let file = file_label(path);
        let push = |label: &str, kind: &'static str, out: &mut Vec<AliasUse>| {
            if let Some(alias) = alias_reference(label) {
                out.push(AliasUse {
                    alias: alias.to_string(),
                    label: label.to_string(),
                    file: file.clone(),
                    kind,
                });
            }
        };
        for label in sch.labels.iter() {
            push(&label.text, "label", &mut out);
        }
        for label in sch.global_labels.iter() {
            push(&label.text, "global_label", &mut out);
        }
        for label in sch.hierarchical_labels.iter() {
            push(&label.text, "hierarchical_label", &mut out);
        }
        for sheet in sch.sheets.iter() {
            for pin in &sheet.pins {
                push(&pin.name, "sheet_pin", &mut out);
            }
        }
    }
    out
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Map a signed (dx, dy) tick offset onto KiCad's four bus-entry directions.
fn entry_dir(dx: f64, dy: f64) -> BusEntryDirection {
    match (dx > 0.0, dy > 0.0) {
        (true, true) => BusEntryDirection::DownRight,
        (false, true) => BusEntryDirection::DownLeft,
        (true, false) => BusEntryDirection::UpRight,
        (false, false) => BusEntryDirection::UpLeft,
    }
}

fn parse_direction(s: &str) -> Option<BusEntryDirection> {
    Some(match s {
        "down_right" => BusEntryDirection::DownRight,
        "down_left" => BusEntryDirection::DownLeft,
        "up_right" => BusEntryDirection::UpRight,
        "up_left" => BusEntryDirection::UpLeft,
        _ => return None,
    })
}

fn read_xy_pair(v: &serde_json::Value) -> Result<(f64, f64, f64, f64), CallToolResult> {
    Ok((
        require_f64(v, "x1")?,
        require_f64(v, "y1")?,
        require_f64(v, "x2")?,
        require_f64(v, "y2")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet_with_bus() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bus.kicad_sch");
        let mut content = String::from(
            "(kicad_sch\n  (version 20260306)\n  (uuid \"11111111-1111-4111-8111-111111111111\")\n",
        );
        content.push_str(&format_bus(100.33, 100.33, 150.11, 100.33));
        content.push_str("\n)\n");
        std::fs::write(&path, content).unwrap();
        (directory, path)
    }

    async fn entry_response(path: &std::path::Path, x: f64, y: f64) -> serde_json::Value {
        let result = handle_add_bus_entry(
            &json!({ "schematic": path, "x": x, "y": y, "direction": "down_right" }),
            &crate::tools::ToolContext::new(
                crate::tools::ServerConfig::default(),
                std::sync::Arc::new(crate::router::ToolRouter::new()),
            ),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "add_bus_entry failed");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        serde_json::from_str(text).unwrap()
    }

    /// #329: which end touches the bus is a fact about the sheet. The caller
    /// here places the entry's `at` ON the bus (contrary to the documented
    /// x/y-is-the-wire-side convention, but exactly how people use it), and
    /// the response must label the corners from the geometry, not the
    /// convention — the old response called the on-bus corner `wire_side`,
    /// and a caller following it attached the wire to the bus itself, where
    /// it connects nothing.
    #[tokio::test]
    async fn bus_side_is_the_corner_that_actually_touches_the_bus() {
        let (_directory, path) = sheet_with_bus();
        let response = entry_response(&path, 120.65, 100.33).await;
        assert_eq!(response["bus_side"]["x"], 120.65, "{response}");
        assert_eq!(response["bus_side"]["y"], 100.33);
        assert_eq!(response["wire_side"]["x"], 123.19);
        assert_eq!(response["wire_side"]["y"], 102.87);
        assert!(
            response.get("note").is_none(),
            "geometry decided: {response}"
        );
    }

    /// The documented usage — x/y at the wire end, the tick running to the
    /// bus — keeps its labels, now confirmed by geometry rather than assumed.
    #[tokio::test]
    async fn the_documented_wire_side_placement_keeps_its_labels() {
        let (_directory, path) = sheet_with_bus();
        let response = entry_response(&path, 120.65, 97.79).await;
        assert_eq!(response["wire_side"]["x"], 120.65, "{response}");
        assert_eq!(response["wire_side"]["y"], 97.79);
        assert_eq!(response["bus_side"]["x"], 123.19);
        assert_eq!(response["bus_side"]["y"], 100.33);
        assert!(response.get("note").is_none(), "{response}");
    }

    /// With no bus near the entry, geometry cannot decide — the response
    /// falls back to the documented convention and says so instead of
    /// silently asserting.
    #[tokio::test]
    async fn a_floating_entry_reports_the_convention_and_says_so() {
        let (_directory, path) = sheet_with_bus();
        let response = entry_response(&path, 30.0, 30.0).await;
        assert_eq!(response["wire_side"]["x"], 30.0, "{response}");
        assert_eq!(response["bus_side"]["x"], 32.54);
        let note = response["note"].as_str().expect("note present");
        assert!(note.contains("no bus touches"), "{note}");
    }

    #[test]
    fn bus_sexp_is_a_bus_node_not_a_wire() {
        let s = format_bus(10.0, 20.0, 50.0, 20.0);
        assert!(s.starts_with("(bus"), "got {s}");
        assert!(s.contains("(xy 10 20) (xy 50 20)"));
    }

    /// `size` is the offset to the bus side, so the two ends are `at` and
    /// `at + size`. Getting the sign wrong puts the tick on the far side of the
    /// bus, where it connects nothing.
    #[test]
    fn entry_direction_follows_the_offset_signs() {
        assert!(matches!(
            entry_dir(2.54, 2.54),
            BusEntryDirection::DownRight
        ));
        assert!(matches!(
            entry_dir(-2.54, 2.54),
            BusEntryDirection::DownLeft
        ));
        assert!(matches!(entry_dir(2.54, -2.54), BusEntryDirection::UpRight));
        assert!(matches!(entry_dir(-2.54, -2.54), BusEntryDirection::UpLeft));
    }

    #[test]
    fn direction_names_round_trip() {
        for (name, dir) in [
            ("down_right", BusEntryDirection::DownRight),
            ("down_left", BusEntryDirection::DownLeft),
            ("up_right", BusEntryDirection::UpRight),
            ("up_left", BusEntryDirection::UpLeft),
        ] {
            assert_eq!(parse_direction(name).unwrap().size(), dir.size());
        }
        assert!(parse_direction("sideways").is_none());
    }

    // ── Bus aliases ──────────────────────────────────────────────────────────

    /// KiCad's subscript markup puts braces inside a name, so the group has to
    /// be matched from the end with a depth counter. `I_{2}C1{I_{2}C}` is a
    /// real label from the cm4-baseboard reference design: prefix `I_{2}C1`,
    /// alias `I_{2}C`. First-brace or last-brace scanning gets both wrong.
    #[test]
    fn group_bus_split_survives_subscript_markup() {
        assert_eq!(split_group_bus("DSI1{DSI}"), Some(("DSI1", "DSI")));
        assert_eq!(
            split_group_bus("I_{2}C1{I_{2}C}"),
            Some(("I_{2}C1", "I_{2}C"))
        );
        assert_eq!(split_group_bus("{A B C}"), Some(("", "A B C")));
        assert_eq!(split_group_bus("SPI6_MOSI"), None);
        assert_eq!(split_group_bus("X_DIG[1..6]"), None);
    }

    /// Active-low names are markup, not buses. Found by running the check over
    /// a real board: `~{RESET}`, `~{CS_ACCEL}` and four more were reported as
    /// references to aliases nothing defines.
    #[test]
    fn text_markup_is_not_a_group_bus() {
        for markup in ["~{RESET}", "~{CS_ACCEL}", "~{GPS_SAFEBOOT}", "_{sub}", "^{sup}"] {
            assert_eq!(split_group_bus(markup), None, "{markup} is markup");
            assert_eq!(alias_reference(markup), None, "{markup} names no alias");
        }
        // A name may still END in markup and carry a real group before it.
        assert_eq!(alias_reference("I_{2}C1{I_{2}C}"), Some("I_{2}C"));
    }

    /// An inner token with whitespace is an explicit member list, not an alias.
    #[test]
    fn only_a_single_token_names_an_alias() {
        assert_eq!(alias_reference("DSI1{DSI}"), Some("DSI"));
        assert_eq!(alias_reference("I_{2}C1{I_{2}C}"), Some("I_{2}C"));
        assert_eq!(alias_reference("{A B C}"), None);
        assert_eq!(alias_reference("BUS{A B}"), None);
        assert_eq!(alias_reference("SPI6_MOSI"), None);
    }

    fn sheet_with_lib_symbols() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aliases.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(uuid \"11111111-1111-4111-8111-111111111111\")\n\
             \t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Description\" \"a ) paren\")\n\t\t)\n\t)\n\
             \t(junction\n\t\t(at 10 10)\n\t)\n)\n",
        )
        .unwrap();
        (directory, path)
    }

    /// A macro rather than a function: a fn-pointer returning a boxed future
    /// cannot borrow its arguments for the caller's lifetime.
    macro_rules! call_json {
        ($handler:path, $args:expr) => {{
            let ctx = crate::tools::ToolContext::new(
                crate::tools::ServerConfig::default(),
                std::sync::Arc::new(crate::router::ToolRouter::new()),
            );
            let args = $args;
            let result = $handler(&args, &ctx).await.unwrap();
            let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
                panic!("expected text");
            };
            assert!(!result.is_error, "handler returned an error: {text}");
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        }};
    }

    /// The alias lands after `lib_symbols`, where eeschema writes it — and the
    /// `)` inside a Description field must not be mistaken for the end of that
    /// block, which is why close_of_node tracks quoted strings.
    /// Regression: the opener patterns carry `\n` and the indent, so the `(`
    /// they name is two bytes in, not one. close_of_node refuses a start that
    /// is not on a `(`, so the off-by-one silently became "nowhere to write".
    #[test]
    fn insert_offset_lands_on_the_lib_symbols_paren() {
        let (_dir, path) = sheet_with_lib_symbols();
        let content = std::fs::read_to_string(&path).unwrap();
        let at = alias_insert_offset(&content).expect("an insertion point");
        assert!(at > content.find("(lib_symbols").unwrap());
        assert!(at < content.find("(junction").unwrap());
    }

    #[tokio::test]
    async fn alias_is_written_after_lib_symbols() {
        let (_dir, path) = sheet_with_lib_symbols();
        let response = call_json!(handle_add_bus_alias, json!({ "schematic": path, "name": "DSI", "members": ["D0_P", "D0_N", "C_P", "C_N"] }));
        assert_eq!(response["action"], "added", "{response}");
        assert_eq!(response["reference_form"], "PREFIX{DSI}");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("(bus_alias \"DSI\"\n\t\t(members \"D0_P\" \"D0_N\" \"C_P\" \"C_N\")\n\t)"),
            "{text}"
        );
        let alias_at = text.find("(bus_alias").unwrap();
        assert!(alias_at > text.find("(lib_symbols").unwrap(), "before lib_symbols");
        assert!(alias_at < text.find("(junction").unwrap(), "after the drawing starts");
    }

    /// Redefining replaces. Two blocks for one alias is a file KiCad resolves
    /// by whichever it reaches first.
    #[tokio::test]
    async fn redefining_an_alias_replaces_it() {
        let (_dir, path) = sheet_with_lib_symbols();
        let args = |members: serde_json::Value| json!({ "schematic": path, "name": "UART", "members": members });
        call_json!(handle_add_bus_alias, args(json!(["TX", "RX"])));
        let response = call_json!(handle_add_bus_alias, args(json!(["TX", "RX", "RTS"])));

        assert_eq!(response["action"], "replaced", "{response}");
        assert_eq!(response["previous_members"], json!(["TX", "RX"]));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("(bus_alias").count(), 1, "{text}");
        assert!(text.contains("\"TX\" \"RX\" \"RTS\""), "{text}");
    }

    #[tokio::test]
    async fn a_member_with_whitespace_is_refused() {
        let (_dir, path) = sheet_with_lib_symbols();
        let ctx = crate::tools::ToolContext::new(
            crate::tools::ServerConfig::default(),
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );
        let result = handle_add_bus_alias(
            &json!({ "schematic": path, "name": "X", "members": ["OK", "not ok"] }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    /// A parent defining the alias and a child using it is the shape the
    /// cm4-baseboard reference uses, and it is legal — the check must not
    /// flag it. The unresolved one next door must be flagged, because KiCad
    /// will not.
    fn hierarchy_using(alias_defined: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root.kicad_sch");
        let child = directory.path().join("child.kicad_sch");
        let definition = if alias_defined {
            "\t(bus_alias \"DSI\"\n\t\t(members \"D0_P\" \"D0_N\")\n\t)\n"
        } else {
            ""
        };
        std::fs::write(
            &root,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(uuid \"22222222-2222-4222-8222-222222222222\")\n\
                 {definition}\
                 \t(sheet\n\t\t(at 50 50)\n\t\t(size 30 20)\n\t\t(uuid \"33333333-3333-4333-8333-333333333333\")\n\
                 \t\t(property \"Sheetname\" \"Child\" (at 50 49 0))\n\
                 \t\t(property \"Sheetfile\" \"child.kicad_sch\" (at 50 71 0))\n\
                 \t\t(pin \"DSI1{{DSI}}\" input (at 50 55 180) (uuid \"44444444-4444-4444-8444-444444444444\"))\n\
                 \t)\n)\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &child,
            "(kicad_sch\n\t(version 20260306)\n\t(uuid \"55555555-5555-4555-8555-555555555555\")\n\
             \t(hierarchical_label \"DSI1{DSI}\"\n\t\t(shape input)\n\t\t(at 40 40 180)\n\t\t(uuid \"66666666-6666-4666-8666-666666666666\")\n\t)\n)\n",
        )
        .unwrap();
        (directory, root)
    }

    #[tokio::test]
    async fn an_alias_defined_in_the_parent_and_used_in_a_child_is_fine() {
        let (_dir, root) = hierarchy_using(true);
        let response = call_json!(handle_validate_bus_aliases, json!({ "schematic": root }));
        assert_eq!(response["sheets_scanned"], 2, "{response}");
        assert_eq!(response["alias_references"], 2, "sheet pin + child label");
        assert_eq!(response["issue_count"], 0, "{response}");
    }

    /// The whole point: KiCad reports nothing here, it just nets up
    /// `DSI1.DSI` and the netlist is quietly wrong.
    #[tokio::test]
    async fn an_alias_nothing_defines_is_reported() {
        let (_dir, root) = hierarchy_using(false);
        let response = call_json!(handle_validate_bus_aliases, json!({ "schematic": root }));
        assert_eq!(response["issue_count"], 1, "{response}");
        assert_eq!(response["issues"][0]["kind"], "unresolved_alias");
        assert_eq!(response["issues"][0]["alias"], "DSI");
        let sites = response["issues"][0]["used_at"].as_array().unwrap();
        assert_eq!(sites.len(), 2, "{response}");
    }

    #[tokio::test]
    async fn list_reports_which_file_defines_each_alias() {
        let (_dir, root) = hierarchy_using(true);
        let response = call_json!(handle_list_bus_aliases, json!({ "schematic": root }));
        assert_eq!(response["alias_count"], 1, "{response}");
        assert_eq!(response["aliases"][0]["name"], "DSI");
        assert_eq!(response["aliases"][0]["defined_in"][0], "root.kicad_sch");
        assert_eq!(response["aliases"][0]["consistent"], true);
    }

    #[test]
    fn all_seven_tools_are_registered() {
        let names: Vec<_> = tools().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "add_bus",
                "batch_add_bus",
                "add_bus_entry",
                "connect_pins_to_bus",
                "add_bus_alias",
                "list_bus_aliases",
                "validate_bus_aliases"
            ]
        );
    }
}
