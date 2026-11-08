//! The screen: the toolbar, two panels, the F-key bar, dialogs, the progress of an operation, the viewer.
//! Only layout and bindings here; what is shown lives in the data the app writes.

use uison::ui::Json;
use uison::ui::dsl::*;

/// Row height in the panels.
pub const ROW_H: f32 = 20.0;
/// Line height in the viewer.
pub const VIEW_LINE_H: f32 = 16.0;
/// Viewer chrome above and below the lines (header and footer).
pub const VIEW_CHROME_H: f32 = 52.0;

fn eq(a: &str, b: &str) -> String {
    format!(r#"{{ eq: ["{a}", "{b}"] }}"#)
}

/// A column cell of a row: fixed width, one line.
fn cell(t: &str, w: &str, align: &str, color: &Json) -> Node {
    text(t).w(w).max_lines(1).text_align(align).color(color.clone()).size(12)
}

/// How the cursor and the selection look: accent; `UISON_LOOK=classic` (TC red) or `amber` (Far).
struct Look {
    cursor: &'static str,
    sel_text: &'static str,
    sel_fill: Option<&'static str>,
}

fn look() -> Look {
    match std::env::var("UISON_LOOK").as_deref() {
        // As in TC: selected red, the cursor the theme's selection color.
        Ok("classic") => Look { cursor: "$theme.selection", sel_text: "$theme.danger", sel_fill: None },
        // Far / NC-like: selected names amber, the cursor a clear grey-blue bar.
        Ok("amber") => Look { cursor: "#8E9BB04D", sel_text: "#E08A00", sel_fill: Some("#E08A0014") },
        // The default (Task 2): the accent color — a blue cursor bar, selected names blue on a light blue tint.
        // Red read as an error; this is what a Mac list looks like.
        _ => Look { cursor: "#0A84FF47", sel_text: "$theme.primary", sel_fill: Some("#0A84FF1A") },
    }
}

fn panel(side: &str) -> Node {
    let s = side;
    let l = look();
    // The cursor: filled in the active panel, outlined in the other one; a selected row has a tint of its own.
    let on_cursor = eq("@i", &format!("@{s}.cursor"));
    let fill = json(&format!(
        r#"{{ switch: {on_cursor}, cases: {{ true: {{ switch: "@{s}.active", cases: {{ true: "{cur}" }}, default: null }} }}, default: {{ switch: "@r.sel", cases: {{ true: {tint} }}, default: {{ state: {{ normal: null, hover: "$theme.surfaceHover" }} }} }} }}"#,
        cur = l.cursor,
        tint = l.sel_fill.map_or("null".to_string(), |t| format!("\"{t}\"")),
    ));
    let stroke = json(&format!(
        r#"{{ switch: {on_cursor}, cases: {{ true: {{ switch: "@{s}.active", cases: {{ true: null }}, default: "$theme.borderHover" }} }}, default: null }}"#
    ));
    // Selected — in the look's color; hidden — dimmed.
    let color = json(&format!(
        r#"{{ switch: "@r.sel", cases: {{ true: "{sel}" }}, default: {{ switch: "@r.hid", cases: {{ true: "$theme.muted" }}, default: "$theme.text" }} }}"#,
        sel = l.sel_text
    ));
    let muted = json(&format!(r#"{{ switch: "@r.sel", cases: {{ true: "{sel}" }}, default: "$theme.muted" }}"#, sel = l.sel_text));
    let item = container()
        .key(format!("{s}-r{{@i}}"))
        .h(ROW_H)
        .fill(fill)
        .stroke(stroke)
        .width(1)
        .on("click", "row")
        .context_menu(format!("@{s}.menu"))
        .child(row().w("fill").h("fill").padding([0, 6]).gap(6).cross("center").children([
            image("@r.i").w(16).h(16).fit("contain"),
            text("@r.n").flex(1).max_lines(1).size(12).color(color.clone()).prop("bold", "@r.dir").tooltip("@r.t"),
            cell("@r.e", &format!("@colw.{s}.ext"), "left", &color),
            cell("@r.s", &format!("@colw.{s}.size"), "right", &color),
            cell("@r.d", &format!("@colw.{s}.time"), "left", &muted),
            cell("@r.a", &format!("@colw.{s}.attr"), "left", &muted),
        ]));
    let rows = list(&format!("{s}.rows"), "r", ROW_H, item).index("i").key(format!("{s}-list")).flex(1).w("fill");
    // Column titles come from the data (`left.h.name` = "Name ▲"): the sorted one shows its direction.
    let head = |k: &str, w: Option<&str>, align: &str| {
        let t = text(format!("@{s}.h.{k}")).size(11).color("$theme.muted").max_lines(1).text_align(align).w("fill");
        let c = container().padding([4, 0]).child(t);
        let c = match w {
            Some(w) => c.w(w),
            None => c.flex(1),
        };
        c.key(format!("{s}-col-{k}")).on("click", "sortcol").cursor("pointer")
    };
    // The left edge of a column's title: drag it to make the column wider or narrower (this panel only).
    // A line through the whole title row; under the pointer it turns accent and thicker.
    let grip = |k: &str| {
        container()
            .key(format!("{s}-cw-{k}"))
            .w(9)
            .align("center")
            .cursor("resize-x")
            .interactive(true)
            .drag(json(&format!(r#"{{ path: "@colw.{s}.{k}", min: 24, max: 480, delta: -1 }}"#)))
            .on("click", "colw")
            .child(
                container()
                    .w(json(r#"{ state: { normal: 1, hover: 2, pressed: 2 } }"#))
                    .h("fill")
                    .fill(json(r#"{ state: { normal: "$theme.borderHover", hover: "$theme.primary", pressed: "$theme.primary" } }"#)),
            )
    };
    let title_fill = json(&format!(r#"{{ switch: "@{s}.active", cases: {{ true: "$theme.primary" }}, default: "$theme.card" }}"#));
    let title_color = json(&format!(r#"{{ switch: "@{s}.active", cases: {{ true: "$theme.onPrimary" }}, default: "$theme.muted" }}"#));
    // The panels share the width half and half.
    column().flex(1).h("fill").cross("stretch").children([
        container()
            .key(format!("{s}-hdr"))
            .fill(title_fill)
            .padding([4, 8])
            .on("click", "header")
            .cursor("pointer")
            .tooltip(t!("Alt+F1 / Alt+F2: source and drive"))
            .child(row().gap(6).cross("center").children([
                image(json(&format!(
                    r#"{{ switch: "@{s}.active", cases: {{ true: "@{s}.iconW" }}, default: {{ switch: "@look.dark", cases: {{ true: "@{s}.iconD" }}, default: "@{s}.iconL" }} }}"#
                )))
                .w(15)
                .h(15)
                .fit("contain"),
                // The source, then the path as names to click (each opens its folder) between plain slashes.
                row().gap(0).flex(1).cross("center").children([
                text(format!("@{s}.srcLabel")).size(12).bold().max_lines(1).color(title_color.clone()),
                container().flex(1).clip(true).child(row().gap(0).children([for_each(
                        &format!("{s}.crumbs"),
                        "cr",
                        text("@cr.t")
                            .key(format!("{s}-cr{{@ci}}"))
                            .size(12)
                            .bold()
                            .max_lines(1)
                            .color(title_color.clone())
                            .cursor(json(r#"{ switch: "@cr.go", cases: { true: "pointer" }, default: "arrow" }"#))
                            .on("click", "crumb"),
                    )
                    .prop("index", "ci")])),
                ]),
            ])),
        // Under the title: the panel, and over it the drop-down of sources and drives (Alt+F1/F2).
        stack().flex(1).w("fill").children([
        column().w("fill").h("fill").cross("stretch").children([
        row().padding([0, 6]).gap(0).cross("stretch").children([
            container().w(22),
            head("name", None, "left"),
            grip("ext"),
            head("ext", Some(&format!("@colw.{s}.ext")), "left"),
            grip("size"),
            head("size", Some(&format!("@colw.{s}.size")), "right"),
            grip("time"),
            head("time", Some(&format!("@colw.{s}.time")), "left"),
            grip("attr"),
            container().padding([4, 0]).w(format!("@colw.{s}.attr")).child(text(t!("Attr")).size(11).color("$theme.muted")),
        ]),
        container().h(1).fill("$theme.divider"),
        // A folder that couldn't be read: a strip above the rows with Retry; the panel stays where it was.
        container().w("fill").padding([6, 6, 0, 6]).visible(format!("@{s}.error")).child(
            container()
                .w("fill")
                .padding([6, 10])
                .radius(6)
                .fill(json(r##"{ switch: "@look.dark", cases: { true: "#3B2222" }, default: "#FDECEA" }"##))
                .stroke("$theme.danger")
                .width(1)
                .child(row().gap(8).cross("center").children([
                    sym("exclamationmark.triangle", 14.0),
                    text(format!("@{s}.error")).size(12).flex(1).max_lines(2),
                    button(t!("Retry")).key(format!("{s}-retry")).variant("secondary").size("sm").on("click", "panelErr"),
                    button("×").key(format!("{s}-errclose")).variant("text").size("sm").tooltip(t!("Close")).on("click", "panelErr"),
                ])),
        ),
        stack().flex(1).w("fill").children([
            rows,
            container().w("fill").padding(12).visible(format!("@{s}.loading")).child(text(t!("Loading…")).color("$theme.muted")),
            container()
                .w("fill")
                .padding([ROW_H + 14.0, 12.0])
                .visible(format!("@{s}.empty"))
                .child(text(t!("Folder is empty")).size(12).color("$theme.muted")),
        ]),
        bar(row().gap(6).children([text(t!("Quick search:")).size(12).color("$theme.muted"), text(format!("@{s}.quick")).size(12).bold()]))
            .visible(format!("@{s}.quickOn"))
            .padding([3, 8]),
        container().h(1).fill("$theme.divider"),
        container().padding([3, 8]).child(text(format!("@{s}.status")).size(11).color("$theme.muted").max_lines(1)),
        ]),
        drop_down(s),
        ]),
    ])
}

/// Sources and drives of a panel (Alt+F1/F2, a click on its title): a list under the title, one click opens
/// a row; a click beside it closes it. The rows are the list dialog's (`dlg.items`), filled in as they come.
fn drop_down(s: &str) -> Node {
    let fill = json(&format!(
        r#"{{ switch: {}, cases: {{ true: "$theme.selection" }}, default: {{ state: {{ normal: null, hover: "$theme.surfaceHover" }} }} }}"#,
        eq("@j", "@dlg.sel")
    ));
    let item = container()
        .key(format!("{s}-dd{{@j}}"))
        .padding([5, 10])
        .radius(5)
        .fill(fill)
        .cursor(json(r#"{ switch: "@it.h", cases: { true: "arrow" }, default: "pointer" }"#))
        .on("click", "dropPick")
        .child(
            row().gap(12).cross("center").children([
                text("@it.label")
                    .flex(1)
                    .max_lines(1)
                    .size(json(r#"{ switch: "@it.h", cases: { true: 11 }, default: 13 }"#))
                    .prop("bold", "@it.h")
                    .color(json(r#"{ switch: "@it.h", cases: { true: "$theme.muted" }, default: "$theme.text" }"#)),
                text("@it.note").color("$theme.muted").size(11).max_lines(1),
            ]),
        );
    container()
        .key(format!("{s}-drop"))
        .w("fill")
        .h("fill")
        .visible(json(&eq("@drop.side", s)))
        .interactive(true)
        .on("click", "dropClose")
        .align("topLeft")
        .padding([2, 6])
        .child(
            container()
                .key(format!("{s}-dropbox"))
                .w(400)
                .fill("$theme.card")
                .stroke("$theme.border")
                .width(1)
                .radius(8)
                .padding(4)
                .interactive(true)
                .on("click", "dropKeep")
                .child(scroll(column().cross("stretch").children([for_each("dlg.items", "it", item).prop("index", "j")])).h("@dlg.listH")),
        )
}

/// A strip on the card color: a Row has no fill of its own.
fn bar(child: Node) -> Node {
    container().fill("$theme.card").child(child)
}

/// An SF Symbol in the ink of the current theme (`crate::icons::symbol`). Where there are none (not a Mac), a
/// character in its place for the buttons that have no word (`stand_in`), nothing for the rest.
fn sym(name: &str, size: f32) -> Node {
    let (light, dark) = (crate::icons::symbol(name, false), crate::icons::symbol(name, true));
    if light.is_empty()
        && let Some(c) = stand_in(name)
    {
        return text(c).size(size).max_lines(1);
    }
    let src = json(&format!(r#"{{ switch: "@look.dark", cases: {{ true: "{dark}" }}, default: "{light}" }}"#));
    image(src).w(size).h(size).fit("contain").visible(!light.is_empty())
}

/// The character for a symbol that is a button by itself (back, settings, the themes).
fn stand_in(name: &str) -> Option<&'static str> {
    Some(match name {
        "chevron.left" => "‹",
        "chevron.right" => "›",
        // Not ⚙ and ☀: those come out as color emoji.
        "gearshape" => "⛭",
        "sun.max" => "☼",
        "moon" => "☾",
        "circle.lefthalf.filled" => "◐",
        _ => return None,
    })
}

fn keybar() -> Node {
    // (command key, key shown, symbol, action, needs a writable panel)
    let quit = if cfg!(target_os = "macos") { ("cmd+q", "⌘Q") } else { ("alt+f4", "Alt+F4") };
    let keys = [
        ("f3", "F3", "eye", "View", false),
        ("f4", "F4", "pencil", "Edit", false),
        ("f5", "F5", "doc.on.doc", "Copy", false),
        ("f6", "F6", "arrow.right.doc.on.clipboard", "Move", true),
        ("f7", "F7", "folder.badge.plus", "New folder", true),
        ("f8", "F8", "trash", "Delete", true),
        (quit.0, quit.1, "power", "Quit", false),
    ];
    let mut children = Vec::new();
    for (i, (k, key, symbol, action, writes)) in keys.iter().enumerate() {
        if i > 0 {
            children.push(separator());
        }
        // The key as a quiet label, the action in plain text, as TC; what a read-only source can't do is dimmed.
        let color = if *writes {
            json(r#"{ switch: "@keys.ro", cases: { true: "$theme.muted" }, default: "$theme.text" }"#)
        } else {
            json(r#""$theme.text""#)
        };
        // What it does and its key, from the key table (as F1 says it).
        let help = crate::keymap::table().into_iter().find(|b| b.keys.contains(k)).map(|b| b.help).unwrap_or(*action);
        let help = t!(help);
        children.push(
            container()
                .key(format!("fk-{k}"))
                .tooltip(format!("{help} ({key})"))
                .flex(1)
                .h(26)
                .radius(5)
                .fill(json(r#"{ state: { normal: null, hover: "$theme.surfaceHover" } }"#))
                .cursor("pointer")
                .on("click", "fkey")
                .child(row().main("center").cross("center").gap(5).h("fill").children([
                    text(*key).size(11).bold().color("$theme.muted"),
                    sym(symbol, 14.0),
                    text(t!(action)).size(12).color(color),
                ])),
        );
    }
    bar(row().gap(4).cross("center").children(children)).padding([3, 6])
}

/// The line between the panels.
fn divider() -> Node {
    container().w(1).h("fill").fill("$theme.divider")
}

/// A thin vertical line between buttons.
fn separator() -> Node {
    container().w(1).h(16).fill("$theme.divider")
}

/// A toolbar button: a symbol, a word (or none), a tip with its key.
fn tool(k: &str, symbol: &str, label: &str, tip: &str) -> Node {
    let mut parts = vec![sym(symbol, 15.0)];
    if !label.is_empty() {
        parts.push(text(label).size(12));
    }
    container()
        .key(format!("tb-{k}"))
        .padding([4, 8])
        .radius(5)
        .fill(json(r#"{ state: { normal: null, hover: "$theme.surfaceHover" } }"#))
        .cursor("pointer")
        .tooltip(tip)
        .on("click", "tool")
        .child(row().gap(6).cross("center").children(parts))
}

/// The buttons at the top: commands on the active panel.
fn toolbar() -> Node {
    let (back, fwd) = if cfg!(target_os = "macos") { ("⌥←", "⌥→") } else { ("Alt+←", "Alt+→") };
    let mut children = vec![
        tool("back", "chevron.left", "", &t!("Back to the previous folder ({})", back)),
        tool("forward", "chevron.right", "", &t!("Forward ({})", fwd)),
        separator(),
        tool("refresh", "arrow.clockwise", t!("Refresh"), t!("Read the active panel again (Ctrl+R)")),
    ];
    // Storages: one button, a menu — a new connection of each kind, and the remembered ones (`conn.menu`).
    if !crate::connect::all().is_empty() {
        children.push(separator());
        children.push(tool("connect", "network", t!("Connect…"), t!("A new connection, or the remembered ones (Ctrl+F)")).context_menu("@conn.menu"));
    }
    children.push(spacer());
    // A connection being made: says so without blocking anything, and can be given up.
    children.push(text("@conn.text").size(12).color("$theme.muted").visible("@conn.on"));
    children.push(
        button(t!("Cancel"))
            .key("tb-cancel-connect")
            .variant("text")
            .size("sm")
            .visible("@conn.on")
            .tooltip(t!("Stop waiting for this connection"))
            .on("click", "tool"),
    );
    // Settings: the theme and the language, in a panel under the button (`settings`).
    children.push(tool("settings", "gearshape", "", t!("Settings: theme, language")));
    // Help: a menu of the keys and About.
    children.push(tool("help", "questionmark.circle", t!("Help"), t!("Keys (F1), about the program")).context_menu(json(&format!(
        r#"[{{ label: "{}", action: "menuKeys", shortcut: "F1" }}, "-", {{ label: "{}", action: "menuAbout" }}]"#,
        t!("Keys"),
        t!("About uison-commander")
    ))));
    bar(row().gap(4).cross("center").children(children)).padding([3, 6])
}

/// Settings (the gear in the toolbar): the theme as three symbols, the language by its own name. A panel
/// under the toolbar at the right; a click beside it (or on the gear again) closes it, a choice keeps it open.
fn settings() -> Node {
    let choice = |k: &str, on: &str, tip: &str, child: Node| {
        let fill = json(&format!(
            r#"{{ switch: {}, cases: {{ true: "$theme.selection" }}, default: {{ state: {{ normal: null, hover: "$theme.surfaceHover" }} }} }}"#,
            eq(on, k)
        ));
        container().key(format!("set-{k}")).padding([5, 10]).radius(5).fill(fill).cursor("pointer").tooltip(tip).on("click", "setPick").child(child)
    };
    // The choices side by side in a thin frame, like a segmented control.
    let group = |children: Vec<Node>| container().stroke("$theme.border").width(1).radius(7).padding(2).child(row().gap(2).children(children));
    let theme = group(vec![
        choice("light", "@set.theme", t!("Light"), sym("sun.max", 15.0)),
        choice("dark", "@set.theme", t!("Dark"), sym("moon", 15.0)),
        choice("system", "@set.theme", t!("As the system"), sym("circle.lefthalf.filled", 15.0)),
    ]);
    let lang = group(crate::i18n::Lang::ALL.iter().map(|l| choice(l.code(), "@set.lang", l.name(), text(l.name()).size(12))).collect());
    let line = |title: &str, choices: Node| row().gap(16).cross("center").children([text(title).size(12).color("$theme.muted").flex(1), choices]);
    container()
        .overlay(true)
        .visible("@set.open")
        .w("fill")
        .h("fill")
        .interactive(true)
        .on("click", "setClose")
        .align("topRight")
        .padding([31, 6, 0, 0])
        .child(
            container()
                .key("set-box")
                .w(260)
                .fill("$theme.card")
                .stroke("$theme.border")
                .width(1)
                .radius(8)
                .padding([10, 12])
                .interactive(true)
                .on("click", "dropKeep")
                .child(column().gap(10).cross("stretch").children([line(t!("Theme"), theme), line(t!("Language"), lang)])),
        )
}

/// The strip of an operation running in the background.
fn job_strip() -> Node {
    bar(row().gap(8).cross("center").children([
        text("@job.title").size(12).bold(),
        progress_bar().value("@job.allFrac").w(160),
        text("@job.stats").size(11).color("$theme.muted").flex(1).max_lines(1),
        button(t!("Show")).key("job-show").variant("secondary").size("sm").on("click", "jobShow"),
        button(t!("Cancel")).key("job-cancel2").variant("secondary").size("sm").on("click", "jobCancel"),
    ]))
    .visible("@job.strip")
    .padding([4, 8])
}

/// A passing notice in the bottom right corner; a click closes it.
fn toast() -> Node {
    container().overlay(true).visible("@toast.on").w("fill").h("fill").align("bottomRight").padding([0, 16, 60, 0]).child(
        container()
            .key("toast")
            .padding([8, 14])
            .radius(8)
            .fill("$theme.card")
            .stroke("$theme.border")
            .width(1)
            .max_w(460)
            .interactive(true)
            .on("click", "toastOff")
            .child(row().gap(8).cross("center").children([sym("checkmark.circle", 15.0), text("@toast.text").size(12).max_lines(3)])),
    )
}

fn dim(visible: &str, child: Node) -> Node {
    // The dimmed backdrop takes the mouse: nothing behind it is clickable.
    container().overlay(true).visible(visible).w("fill").h("fill").fill("#00000059").align("center").interactive(true).cursor("arrow").child(child)
}

/// A dialog's card: as wide as its widest line, between 440 and 640 (a field fills it); `wide` — up to 960
/// (the keys' rows, a conflict's buttons in Russian).
fn dialog_box(wide: &str, children: Vec<Node>) -> Node {
    container()
        .fill("$theme.card")
        .stroke("$theme.border")
        .width(1)
        .radius(10)
        .padding(18)
        .min_w(440)
        .max_w(json(&format!(r#"{{ switch: "{wide}", cases: {{ true: 960 }}, default: 640 }}"#)))
        .child(column().gap(10).cross("stretch").children(children))
}

fn progress_dialog() -> Node {
    dim(
        "@job.show",
        dialog_box(
            "",
            vec![
                text("@job.title").size(15).bold(),
                text("@job.route").size(11).color("$theme.muted").max_lines(1).visible("@job.route"),
                text("@job.current").size(12).max_lines(2).color("$theme.label"),
                row()
                    .gap(10)
                    .cross("center")
                    .children([text(t!("File")).size(11).color("$theme.muted").w(40), progress_bar().value("@job.fileFrac").flex(1)]),
                row()
                    .gap(10)
                    .cross("center")
                    .children([text(t!("Total")).size(11).color("$theme.muted").w(40), progress_bar().value("@job.allFrac").flex(1)]),
                text("@job.stats").size(11).color("$theme.muted"),
                row().main("end").gap(8).children([
                    button(t!("Background")).key("job-bg").variant("secondary").on("click", "jobBg"),
                    button(t!("Cancel")).key("job-cancel").variant("secondary").on("click", "jobCancel"),
                ]),
            ],
        ),
    )
}

fn dialog() -> Node {
    let kind = |k: &str| eq("@dlg.kind", k);
    let item_fill = json(&format!(
        r#"{{ switch: {}, cases: {{ true: "$theme.selection" }}, default: {{ state: {{ normal: null, hover: "$theme.surfaceHover" }} }} }}"#,
        eq("@j", "@dlg.sel")
    ));
    let items = for_each(
        "dlg.items",
        "it",
        container().key("dlg-it{@j}").padding([5, 8]).radius(5).fill(item_fill).on("click", "dlgPick").cursor("pointer").child(
            row().gap(8).children([
                text("@it.label")
                    .flex(1)
                    .max_lines(1)
                    .size(json(r#"{ switch: "@it.h", cases: { true: 11 }, default: 13 }"#))
                    .prop("bold", "@it.h")
                    .color(json(r#"{ switch: "@it.h", cases: { true: "$theme.muted" }, default: "$theme.text" }"#)),
                text("@it.note").color("$theme.muted").size(11),
            ]),
        ),
    )
    .prop("index", "j");
    let param = column().gap(4).cross("stretch").children([
        text("@p.title").size(12).color("$theme.label"),
        radio_group().value("@p.value").prop("options", "@p.options").visible(json(&eq("@p.kind", "choice"))),
        // A file on this machine (an SSH key): typed, or chosen with the system's dialog.
        row().gap(8).cross("center").visible(json(&eq("@p.kind", "text"))).children([
            text_field().key("dlg-p{@pi}").value("@p.value").placeholder("@p.hint").flex(1).on("submit", "dlgOk"),
            button(t!("Choose…")).key("dlg-br{@pi}").variant("secondary").size("sm").visible("@p.browse").on("click", "dlgBrowse"),
        ]),
        text_field()
            .key("dlg-s{@pi}")
            .value("@p.value")
            .prop("password", true)
            .placeholder("@p.hint")
            .visible(json(&eq("@p.kind", "secret")))
            .on("submit", "dlgOk"),
        checkbox("@p.label").checked("@p.value").visible(json(&eq("@p.kind", "flag"))),
    ]);
    let prop_row = row().gap(10).children([text("@kv.k").w(120).size(12).color("$theme.muted"), text("@kv.v").flex(1).size(12)]);
    let label = |t: &str| text(t).w(120).size(12).color("$theme.muted");
    let name_row = row()
        .gap(10)
        .cross("center")
        .visible("@dlg.nameOn")
        .children([label(t!("Name")), text_field().key("dlg-name").bind("dlg.name").flex(1).on("submit", "dlgOk")]);
    // Permissions: a row per who, a column per what.
    let col = |t: &str| text(t).w(64).size(11).color("$theme.muted");
    // The number (`644`) next to the ticks, following them.
    let mut perm_rows = vec![row().gap(10).cross("center").children([
        label(t!("Permissions")),
        col(t!("Read")),
        col(t!("Write")),
        col(t!("Execute")),
        text("@dlg.octal").key("dlg-octal").size(13).bold().font("Menlo").tooltip(t!("The permissions as a number (chmod)")),
    ])];
    for (who, title) in [("u", "Owner"), ("g", "Group"), ("o", "Others")] {
        let tick = |what: &str| {
            container()
                .w(64)
                .child(checkbox("").key(format!("dlg-perm-{who}{what}")).checked(format!("@dlg.perm.{who}{what}")).on("change", "permTick"))
        };
        perm_rows.push(row().gap(10).cross("center").children([text(t!(title)).w(120).size(12), tick("r"), tick("w"), tick("x")]));
    }
    let perms = column().gap(4).cross("stretch").visible("@dlg.permOn").children(perm_rows);
    // Some at the left edge (Test of a connection form), the rest at the right; the key is the index in all.
    let b = || button("@b.label").key("dlg-b{@b.i}").variant("@b.variant").on("click", "dlgButton");
    let buttons = row().gap(8).visible("!@dlg.side").children([
        row().gap(8).children([for_each("dlg.buttonsL", "b", b())]),
        spacer(),
        row().gap(8).children([for_each("dlg.buttons", "b", b())]),
    ]);
    dim(
        "@dlg.open",
        dialog_box(
            "@dlg.wide",
            vec![
                text("@dlg.title").size(15).bold(),
                text("@dlg.text").size(12).color("$theme.label").visible("@dlg.text"),
                text_field().key("dlg-input").bind("dlg.value").visible(json(&kind("input"))).on("submit", "dlgOk"),
                // The list; the connections have their own buttons beside it.
                row().gap(14).cross("stretch").visible(json(&kind("list"))).children([
                    container()
                        .flex(1)
                        .h("@dlg.listH")
                        .padding(json(r#"{ switch: "@dlg.side", cases: { true: 3 }, default: 0 }"#))
                        .radius(6)
                        .stroke(json(r#"{ switch: "@dlg.side", cases: { true: "$theme.border" }, default: null }"#))
                        .width(1)
                        .child(scroll(column().cross("stretch").children([items]))),
                    connection_buttons(),
                ]),
                column().gap(10).cross("stretch").visible(json(&kind("params"))).children([for_each("dlg.params", "p", param).prop("index", "pi")]),
                column().gap(10).cross("stretch").visible(json(&kind("props"))).children([
                    name_row,
                    column().gap(4).cross("stretch").children([for_each("dlg.props", "kv", prop_row)]),
                    perms,
                ]),
                status_line(),
                buttons,
            ],
        ),
    )
}

/// The connections' buttons, right of the list: Add (a menu of kinds), Edit and Delete of the chosen one, and
/// Close at the bottom.
fn connection_buttons() -> Node {
    let b = |k: &str, label: &str, action: &str| button(label).key(format!("conn-{k}")).variant("secondary").w("fill").on("click", action);
    column().w(150).gap(8).cross("stretch").visible("@dlg.side").children([
        b("add", &format!("{} ▾", t!("Add")), "connAdd").context_menu("@conn.addMenu"),
        b("edit", t!("Edit…"), "connEdit").disabled("@dlg.none"),
        b("delete", t!("Delete"), "connDelete").disabled("@dlg.none"),
        spacer(),
        b("close", t!("Close"), "connClose"),
    ])
}

/// Help → About: a narrow card, centred, the website a link.
fn about() -> Node {
    let site = crate::HOMEPAGE.trim_start_matches("https://");
    dim(
        "@dlg.about",
        container().fill("$theme.card").stroke("$theme.border").width(1).radius(10).padding(20).w(360).child(
            column().gap(6).cross("center").children([
                text("uison-commander").size(22).bold(),
                text(t!("Version {}", env!("CARGO_PKG_VERSION"))).size(13).color("$theme.muted"),
                text(t!(
                    "A two-panel file manager in the spirit of Total Commander: the disk, S3 and SFTP servers side by side, with the keys of TC."
                ))
                .align("center")
                .margin([8, 0, 0, 0]),
                // A link Text rather than the library Link: that one is as wide as the column and starts at its left.
                text(site)
                    .key("about-site")
                    .href(crate::HOMEPAGE)
                    .margin([8, 0, 0, 0])
                    .color(json(r#"{ state: { normal: "$theme.primary", hover: "$theme.primaryHover", pressed: "$theme.primaryPressed" } }"#))
                    .decoration(json(r#"{ state: { normal: "none", hover: "underline", pressed: "underline" } }"#)),
                button(t!("Close")).key("dlg-b0").variant("secondary").margin([10, 0, 0, 0]).on("click", "dlgButton"),
            ]),
        ),
    )
}

/// A form's line above the buttons: "Connecting…", why it failed (red), that it works (green), a question.
fn status_line() -> Node {
    let kind = |k: &str| json(&eq("@dlg.statusKind", k));
    let color = json(
        r#"{ switch: "@dlg.statusKind", cases: { error: "$theme.danger", ok: "$theme.success", busy: "$theme.muted" }, default: "$theme.text" }"#,
    );
    container().key("dlg-status").w("fill").padding([8, 10]).radius(6).fill("$theme.surfaceHover").visible("@dlg.status").child(
        row().gap(8).cross("start").children([
            container().visible(kind("busy")).child(sym("hourglass", 14.0)),
            container().visible(kind("error")).child(sym("exclamationmark.triangle", 14.0)),
            container().visible(kind("ok")).child(sym("checkmark.circle", 14.0)),
            container().visible(kind("ask")).child(sym("questionmark.circle", 14.0)),
            text("@dlg.status").key("dlg-status-text").size(12).flex(1).color(color),
        ]),
    )
}

/// A mode button of the viewer: filled while on.
fn view_mode(k: &str, label: &str, on: &str) -> Node {
    let fill = json(&format!(
        r#"{{ switch: "{on}", cases: {{ true: "$theme.selection" }}, default: {{ state: {{ normal: null, hover: "$theme.surfaceHover" }} }} }}"#
    ));
    container().key(format!("vm-{k}")).padding([3, 10]).radius(5).fill(fill).cursor("pointer").on("click", "viewMode").child(text(label).size(11))
}

fn viewer() -> Node {
    let line = text("@l").font("Menlo").size(12).h(VIEW_LINE_H).max_lines(1).wrap(false);
    container().overlay(true).visible("@view.open").w("fill").h("fill").fill("$theme.background").child(
        column().w("fill").h("fill").cross("stretch").children([
            bar(row().gap(10).cross("center").children([
                text("@view.title").size(12).bold().flex(1).max_lines(1),
                // The modes as buttons too (1, 3, W): the one on is filled.
                view_mode("text", t!("Text"), "@view.text").visible("!@view.isImg"),
                view_mode("hex", t!("Hex"), "@view.hex").visible("!@view.isImg"),
                view_mode("wrap", t!("Wrap"), "@view.wrap").visible("!@view.isImg"),
                text("@view.pos").size(11).color("$theme.muted"),
            ]))
            .padding([6, 10]),
            container()
                .key("view-body")
                .flex(1)
                .w("fill")
                .padding([0, 8])
                .clip(true)
                .on("wheel", "viewWheel")
                .interactive(true)
                .cursor("text")
                .child(
                    stack().w("fill").h("fill").children([
                        column().visible("!@view.isImg").children([for_each("view.lines", "l", line)]),
                        // A picture at its own size (a small one is not blown up into a blur), fitted if larger.
                        container()
                            .w("fill")
                            .h("fill")
                            .align("center")
                            .visible("@view.isImg")
                            .child(image("@view.img").fit("contain").w("@view.imgW").h("@view.imgH")),
                    ]),
                ),
            bar(text("@view.help").size(11).color("$theme.muted")).padding([6, 10]),
        ]),
    )
}

pub fn screen() -> Node {
    stack().w("fill").h("fill").children([
        column().w("fill").h("fill").cross("stretch").children([
            toolbar(),
            container().h(1).fill("$theme.divider"),
            row().flex(1).cross("stretch").children([panel("left"), divider(), panel("right")]),
            job_strip(),
            container().h(1).fill("$theme.divider"),
            keybar(),
        ]),
        toast(),
        settings(),
        viewer(),
        progress_dialog(),
        dialog(),
        about(),
    ])
}
