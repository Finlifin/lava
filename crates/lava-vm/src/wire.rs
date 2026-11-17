//! Bidirectional wire conversion between [`lava_proto`] types and [`interpreter::NativeValue`].
//!
//! Matches the flat array wire layout in `packages/lava_sys/src/lib.ns` (`encode_wire_event`,
//! `decode_wire_event`, `DrawCmd.to_wire`, and `host_commit_frame`).

use interpreter::NativeValue;
use lava_proto::{
    CaretShape, ColorRgba, DrawCmd, MouseAction, PointerStyle, RenderFrame, UiEvent, WindowConfig,
};

/// Pack a [`ColorRgba`] into an `i64` (`0xRRGGBBAA` in the lower 32 bits).
pub fn pack_rgba(color: ColorRgba) -> i64 {
    i64::from(color.0)
}

/// Unpack a Nessa `i64` color into [`ColorRgba`] via `(v as u64 & 0xFFFF_FFFF) as u32`.
pub fn unpack_rgba(packed: i64) -> ColorRgba {
    ColorRgba::from_u32(((packed as u64) & 0xFFFF_FFFF) as u32)
}

/// Encode a platform [`UiEvent`] as a `[tag, ...]` [`NativeValue::List`] matching
/// `decode_wire_event` in `lava_sys`.
pub fn ui_event_to_native(ev: &UiEvent) -> NativeValue {
    match ev {
        UiEvent::Key {
            key,
            modifiers,
            text,
        } => NativeValue::List(vec![
            NativeValue::I64(0),
            NativeValue::Str(key.clone()),
            NativeValue::I64(i64::from(*modifiers)),
            NativeValue::Str(text.clone().unwrap_or_default()),
        ]),
        UiEvent::MouseButton {
            action,
            button,
            x,
            y,
            modifiers,
            press_count,
        } => NativeValue::List(vec![
            NativeValue::I64(1),
            NativeValue::Bool(*action == MouseAction::Press),
            NativeValue::I64(i64::from(*button)),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::I64(i64::from(*modifiers)),
            NativeValue::I64(i64::from(*press_count)),
        ]),
        UiEvent::MouseMove { x, y, modifiers } => NativeValue::List(vec![
            NativeValue::I64(2),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::I64(i64::from(*modifiers)),
        ]),
        UiEvent::MouseScroll {
            dx,
            dy,
            x,
            y,
            modifiers,
        } => NativeValue::List(vec![
            NativeValue::I64(3),
            NativeValue::F64(*dx),
            NativeValue::F64(*dy),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::I64(i64::from(*modifiers)),
        ]),
        UiEvent::Resize {
            width,
            height,
            scale_factor,
        } => NativeValue::List(vec![
            NativeValue::I64(4),
            NativeValue::I64(i64::from(*width)),
            NativeValue::I64(i64::from(*height)),
            NativeValue::I64(i64::from(*scale_factor)),
        ]),
        UiEvent::Focus { focused } => {
            NativeValue::List(vec![NativeValue::I64(5), NativeValue::Bool(*focused)])
        }
        UiEvent::CloseRequested => NativeValue::List(vec![NativeValue::I64(6)]),
        UiEvent::Tick { timer_id, now_ms } => NativeValue::List(vec![
            NativeValue::I64(7),
            NativeValue::I64(*timer_id as i64),
            NativeValue::I64(*now_ms),
        ]),
        UiEvent::AsyncJobDone {
            job_id,
            ok,
            exit_code,
            stdout,
            stderr,
        } => NativeValue::List(vec![
            NativeValue::I64(8),
            NativeValue::I64(*job_id as i64),
            NativeValue::Bool(*ok),
            NativeValue::I64(i64::from(*exit_code)),
            NativeValue::Str(stdout.clone()),
            NativeValue::Str(stderr.clone()),
        ]),
    }
}

/// Decode a `[tag, ...]` [`NativeValue::List`] back into a [`UiEvent`].
pub fn native_to_ui_event(val: &NativeValue) -> Result<UiEvent, String> {
    let items = expect_list(val, "wire UiEvent")?;
    if items.is_empty() {
        return Err("wire UiEvent list must not be empty".to_string());
    }
    let tag = expect_i64(&items[0], "wire UiEvent tag")?;
    match tag {
        0 => {
            require_min_len(items, 4, "UiEvent::Key")?;
            let key = expect_str(&items[1], "UiEvent::Key.key")?;
            let modifiers = expect_u32(&items[2], "UiEvent::Key.modifiers")?;
            let raw_text = expect_str(&items[3], "UiEvent::Key.text")?;
            let text = if raw_text.is_empty() {
                None
            } else {
                Some(raw_text)
            };
            Ok(UiEvent::Key {
                key,
                modifiers,
                text,
            })
        }
        1 => {
            require_min_len(items, 7, "UiEvent::MouseButton")?;
            let is_press = expect_bool(&items[1], "UiEvent::MouseButton.is_press")?;
            let button = expect_u32(&items[2], "UiEvent::MouseButton.button")?;
            let x = expect_f64(&items[3], "UiEvent::MouseButton.x")?;
            let y = expect_f64(&items[4], "UiEvent::MouseButton.y")?;
            let modifiers = expect_u32(&items[5], "UiEvent::MouseButton.modifiers")?;
            let press_count = expect_i32(&items[6], "UiEvent::MouseButton.press_count")?;
            Ok(UiEvent::MouseButton {
                action: if is_press {
                    MouseAction::Press
                } else {
                    MouseAction::Release
                },
                button,
                x,
                y,
                modifiers,
                press_count,
            })
        }
        2 => {
            require_min_len(items, 4, "UiEvent::MouseMove")?;
            let x = expect_f64(&items[1], "UiEvent::MouseMove.x")?;
            let y = expect_f64(&items[2], "UiEvent::MouseMove.y")?;
            let modifiers = expect_u32(&items[3], "UiEvent::MouseMove.modifiers")?;
            Ok(UiEvent::MouseMove { x, y, modifiers })
        }
        3 => {
            require_min_len(items, 6, "UiEvent::MouseScroll")?;
            let dx = expect_f64(&items[1], "UiEvent::MouseScroll.dx")?;
            let dy = expect_f64(&items[2], "UiEvent::MouseScroll.dy")?;
            let x = expect_f64(&items[3], "UiEvent::MouseScroll.x")?;
            let y = expect_f64(&items[4], "UiEvent::MouseScroll.y")?;
            let modifiers = expect_u32(&items[5], "UiEvent::MouseScroll.modifiers")?;
            Ok(UiEvent::MouseScroll {
                dx,
                dy,
                x,
                y,
                modifiers,
            })
        }
        4 => {
            require_min_len(items, 4, "UiEvent::Resize")?;
            let width = expect_i32(&items[1], "UiEvent::Resize.width")?;
            let height = expect_i32(&items[2], "UiEvent::Resize.height")?;
            let scale_factor = expect_i32(&items[3], "UiEvent::Resize.scale_factor")?;
            Ok(UiEvent::Resize {
                width,
                height,
                scale_factor,
            })
        }
        5 => {
            require_min_len(items, 2, "UiEvent::Focus")?;
            let focused = expect_bool(&items[1], "UiEvent::Focus.focused")?;
            Ok(UiEvent::Focus { focused })
        }
        6 => Ok(UiEvent::CloseRequested),
        7 => {
            require_min_len(items, 3, "UiEvent::Tick")?;
            let timer_id = expect_i64(&items[1], "UiEvent::Tick.timer_id")? as u64;
            let now_ms = expect_i64(&items[2], "UiEvent::Tick.now_ms")?;
            Ok(UiEvent::Tick { timer_id, now_ms })
        }
        8 => {
            require_min_len(items, 6, "UiEvent::AsyncJobDone")?;
            let job_id = expect_i64(&items[1], "UiEvent::AsyncJobDone.job_id")? as u64;
            let ok = expect_bool(&items[2], "UiEvent::AsyncJobDone.ok")?;
            let exit_code = expect_i32(&items[3], "UiEvent::AsyncJobDone.exit_code")?;
            let stdout = expect_str(&items[4], "UiEvent::AsyncJobDone.stdout")?;
            let stderr = expect_str(&items[5], "UiEvent::AsyncJobDone.stderr")?;
            Ok(UiEvent::AsyncJobDone {
                job_id,
                ok,
                exit_code,
                stdout,
                stderr,
            })
        }
        other => Err(format!("unknown wire UiEvent tag: {other}")),
    }
}

/// Encode a [`DrawCmd`] into a `[tag, ...]` [`NativeValue::List`] matching `DrawCmd.to_wire`
/// in `lava_sys`.
pub fn draw_cmd_to_native(cmd: &DrawCmd) -> NativeValue {
    match cmd {
        DrawCmd::ClipPush { x, y, w, h } => NativeValue::List(vec![
            NativeValue::I64(0),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::F64(*w),
            NativeValue::F64(*h),
        ]),
        DrawCmd::ClipPop => NativeValue::List(vec![NativeValue::I64(1)]),
        DrawCmd::FillRect {
            x,
            y,
            w,
            h,
            color,
            radius,
        } => NativeValue::List(vec![
            NativeValue::I64(2),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::F64(*w),
            NativeValue::F64(*h),
            NativeValue::I64(pack_rgba(*color)),
            NativeValue::F64(*radius),
        ]),
        DrawCmd::StrokeRect {
            x,
            y,
            w,
            h,
            color,
            line_width,
            radius,
        } => NativeValue::List(vec![
            NativeValue::I64(3),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::F64(*w),
            NativeValue::F64(*h),
            NativeValue::I64(pack_rgba(*color)),
            NativeValue::F64(*line_width),
            NativeValue::F64(*radius),
        ]),
        DrawCmd::Line {
            x1,
            y1,
            x2,
            y2,
            color,
            line_width,
        } => NativeValue::List(vec![
            NativeValue::I64(4),
            NativeValue::F64(*x1),
            NativeValue::F64(*y1),
            NativeValue::F64(*x2),
            NativeValue::F64(*y2),
            NativeValue::I64(pack_rgba(*color)),
            NativeValue::F64(*line_width),
        ]),
        DrawCmd::TextRun {
            x,
            y,
            text,
            font_family,
            font_size_pt,
            color,
            bold,
            italic,
            underline,
        } => NativeValue::List(vec![
            NativeValue::I64(5),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::Str(text.clone()),
            NativeValue::Str(font_family.clone()),
            NativeValue::F64(*font_size_pt),
            NativeValue::I64(pack_rgba(*color)),
            NativeValue::Bool(*bold),
            NativeValue::Bool(*italic),
            NativeValue::Bool(*underline),
        ]),
        DrawCmd::CursorCaret {
            x,
            y,
            w,
            h,
            shape,
            color,
        } => NativeValue::List(vec![
            NativeValue::I64(6),
            NativeValue::F64(*x),
            NativeValue::F64(*y),
            NativeValue::F64(*w),
            NativeValue::F64(*h),
            NativeValue::I64(*shape as i64),
            NativeValue::I64(pack_rgba(*color)),
        ]),
    }
}

/// Parse a single `[tag, ...]` wire command into a [`DrawCmd`].
pub fn parse_draw_cmd(val: &NativeValue) -> Result<DrawCmd, String> {
    let items = expect_list(val, "wire DrawCmd")?;
    if items.is_empty() {
        return Err("wire DrawCmd list must not be empty".to_string());
    }
    let tag = expect_i64(&items[0], "wire DrawCmd tag")?;
    match tag {
        0 => {
            require_min_len(items, 5, "DrawCmd::ClipPush")?;
            Ok(DrawCmd::ClipPush {
                x: expect_f64(&items[1], "ClipPush.x")?,
                y: expect_f64(&items[2], "ClipPush.y")?,
                w: expect_f64(&items[3], "ClipPush.w")?,
                h: expect_f64(&items[4], "ClipPush.h")?,
            })
        }
        1 => Ok(DrawCmd::ClipPop),
        2 => {
            require_min_len(items, 7, "DrawCmd::FillRect")?;
            Ok(DrawCmd::FillRect {
                x: expect_f64(&items[1], "FillRect.x")?,
                y: expect_f64(&items[2], "FillRect.y")?,
                w: expect_f64(&items[3], "FillRect.w")?,
                h: expect_f64(&items[4], "FillRect.h")?,
                color: unpack_rgba(expect_i64(&items[5], "FillRect.color")?),
                radius: expect_f64(&items[6], "FillRect.radius")?,
            })
        }
        3 => {
            require_min_len(items, 8, "DrawCmd::StrokeRect")?;
            Ok(DrawCmd::StrokeRect {
                x: expect_f64(&items[1], "StrokeRect.x")?,
                y: expect_f64(&items[2], "StrokeRect.y")?,
                w: expect_f64(&items[3], "StrokeRect.w")?,
                h: expect_f64(&items[4], "StrokeRect.h")?,
                color: unpack_rgba(expect_i64(&items[5], "StrokeRect.color")?),
                line_width: expect_f64(&items[6], "StrokeRect.line_width")?,
                radius: expect_f64(&items[7], "StrokeRect.radius")?,
            })
        }
        4 => {
            require_min_len(items, 7, "DrawCmd::Line")?;
            Ok(DrawCmd::Line {
                x1: expect_f64(&items[1], "Line.x1")?,
                y1: expect_f64(&items[2], "Line.y1")?,
                x2: expect_f64(&items[3], "Line.x2")?,
                y2: expect_f64(&items[4], "Line.y2")?,
                color: unpack_rgba(expect_i64(&items[5], "Line.color")?),
                line_width: expect_f64(&items[6], "Line.line_width")?,
            })
        }
        5 => {
            require_min_len(items, 10, "DrawCmd::TextRun")?;
            Ok(DrawCmd::TextRun {
                x: expect_f64(&items[1], "TextRun.x")?,
                y: expect_f64(&items[2], "TextRun.y")?,
                text: expect_str(&items[3], "TextRun.text")?,
                font_family: expect_str(&items[4], "TextRun.font_family")?,
                font_size_pt: expect_f64(&items[5], "TextRun.font_size_pt")?,
                color: unpack_rgba(expect_i64(&items[6], "TextRun.color")?),
                bold: expect_bool(&items[7], "TextRun.bold")?,
                italic: expect_bool(&items[8], "TextRun.italic")?,
                underline: expect_bool(&items[9], "TextRun.underline")?,
            })
        }
        6 => {
            require_min_len(items, 7, "DrawCmd::CursorCaret")?;
            Ok(DrawCmd::CursorCaret {
                x: expect_f64(&items[1], "CursorCaret.x")?,
                y: expect_f64(&items[2], "CursorCaret.y")?,
                w: expect_f64(&items[3], "CursorCaret.w")?,
                h: expect_f64(&items[4], "CursorCaret.h")?,
                shape: CaretShape::from_code(expect_i64(&items[5], "CursorCaret.shape")?),
                color: unpack_rgba(expect_i64(&items[6], "CursorCaret.color")?),
            })
        }
        other => Err(format!("unknown wire DrawCmd tag: {other}")),
    }
}

/// Parse `"ui_commit_frame"` arguments `[title, width, height, bg_rgba, pointer_code, wire_cmds]`
/// into a [`RenderFrame`].
pub fn parse_commit_frame_args(args: &[NativeValue]) -> Result<RenderFrame, String> {
    if args.len() != 6 {
        return Err(format!(
            "ui_commit_frame expects 6 arguments, got {}",
            args.len()
        ));
    }
    let title = expect_str(&args[0], "ui_commit_frame.title")?;
    let raw_width = expect_i64(&args[1], "ui_commit_frame.width")?;
    let raw_height = expect_i64(&args[2], "ui_commit_frame.height")?;
    let bg_rgba = unpack_rgba(expect_i64(&args[3], "ui_commit_frame.bg_rgba")?);
    let pointer_code = expect_i64(&args[4], "ui_commit_frame.pointer_code")?;
    let raw_cmds = expect_list(&args[5], "ui_commit_frame.wire_cmds")?;

    let width = raw_width.clamp(1, i64::from(i32::MAX)) as i32;
    let height = raw_height.clamp(1, i64::from(i32::MAX)) as i32;

    let mut commands = Vec::with_capacity(raw_cmds.len());
    for raw_cmd in raw_cmds {
        commands.push(parse_draw_cmd(raw_cmd)?);
    }

    Ok(RenderFrame {
        generation: 0,
        window: WindowConfig {
            title,
            width,
            height,
            background: bg_rgba,
            pointer: PointerStyle::from_code(pointer_code),
        },
        commands,
    })
}

fn next_flat_val<'a>(
    items: &'a [NativeValue],
    cur: &mut usize,
    ctx: &str,
) -> Result<&'a NativeValue, String> {
    let v = items
        .get(*cur)
        .ok_or_else(|| format!("unexpected end of flat DrawCmd stream reading {ctx} at {cur}"))?;
    *cur += 1;
    Ok(v)
}

/// Parse `"ui_commit_frame_flat"` arguments `[title, width, height, bg_rgba, pointer_code, flat_list]`
/// into a [`RenderFrame`] with a single reused Nessa `List` (zero per-command child `List`s or byte loops).
pub fn parse_commit_frame_flat_args(args: &[NativeValue]) -> Result<RenderFrame, String> {
    if args.len() != 6 {
        return Err(format!(
            "ui_commit_frame_flat expects 6 arguments, got {}",
            args.len()
        ));
    }
    let title = expect_str(&args[0], "ui_commit_frame_flat.title")?;
    let raw_width = expect_i64(&args[1], "ui_commit_frame_flat.width")?;
    let raw_height = expect_i64(&args[2], "ui_commit_frame_flat.height")?;
    let bg_rgba = unpack_rgba(expect_i64(&args[3], "ui_commit_frame_flat.bg_rgba")?);
    let pointer_code = expect_i64(&args[4], "ui_commit_frame_flat.pointer_code")?;
    let items = expect_list(&args[5], "ui_commit_frame_flat.flat_list")?;

    let width = raw_width.clamp(1, i64::from(i32::MAX)) as i32;
    let height = raw_height.clamp(1, i64::from(i32::MAX)) as i32;

    let mut commands = Vec::with_capacity(items.len() / 6);
    let mut cur = 0usize;
    let mut last_fam = String::new();

    while cur < items.len() {
        let tag = expect_i64(
            next_flat_val(items, &mut cur, "DrawCmd.tag")?,
            "DrawCmd.tag",
        )?;
        match tag {
            0 => {
                let x = expect_f64(next_flat_val(items, &mut cur, "ClipPush.x")?, "ClipPush.x")?;
                let y = expect_f64(next_flat_val(items, &mut cur, "ClipPush.y")?, "ClipPush.y")?;
                let w = expect_f64(next_flat_val(items, &mut cur, "ClipPush.w")?, "ClipPush.w")?;
                let h = expect_f64(next_flat_val(items, &mut cur, "ClipPush.h")?, "ClipPush.h")?;
                commands.push(DrawCmd::ClipPush { x, y, w, h });
            }
            1 => commands.push(DrawCmd::ClipPop),
            2 => {
                let x = expect_f64(next_flat_val(items, &mut cur, "FillRect.x")?, "FillRect.x")?;
                let y = expect_f64(next_flat_val(items, &mut cur, "FillRect.y")?, "FillRect.y")?;
                let w = expect_f64(next_flat_val(items, &mut cur, "FillRect.w")?, "FillRect.w")?;
                let h = expect_f64(next_flat_val(items, &mut cur, "FillRect.h")?, "FillRect.h")?;
                let color = unpack_rgba(expect_i64(
                    next_flat_val(items, &mut cur, "FillRect.c")?,
                    "FillRect.c",
                )?);
                let radius =
                    expect_f64(next_flat_val(items, &mut cur, "FillRect.r")?, "FillRect.r")?;
                commands.push(DrawCmd::FillRect {
                    x,
                    y,
                    w,
                    h,
                    color,
                    radius,
                });
            }
            3 => {
                let x = expect_f64(
                    next_flat_val(items, &mut cur, "StrokeRect.x")?,
                    "StrokeRect.x",
                )?;
                let y = expect_f64(
                    next_flat_val(items, &mut cur, "StrokeRect.y")?,
                    "StrokeRect.y",
                )?;
                let w = expect_f64(
                    next_flat_val(items, &mut cur, "StrokeRect.w")?,
                    "StrokeRect.w",
                )?;
                let h = expect_f64(
                    next_flat_val(items, &mut cur, "StrokeRect.h")?,
                    "StrokeRect.h",
                )?;
                let color = unpack_rgba(expect_i64(
                    next_flat_val(items, &mut cur, "StrokeRect.c")?,
                    "StrokeRect.c",
                )?);
                let line_width = expect_f64(
                    next_flat_val(items, &mut cur, "StrokeRect.lw")?,
                    "StrokeRect.lw",
                )?;
                let radius = expect_f64(
                    next_flat_val(items, &mut cur, "StrokeRect.r")?,
                    "StrokeRect.r",
                )?;
                commands.push(DrawCmd::StrokeRect {
                    x,
                    y,
                    w,
                    h,
                    color,
                    line_width,
                    radius,
                });
            }
            4 => {
                let x1 = expect_f64(next_flat_val(items, &mut cur, "Line.x1")?, "Line.x1")?;
                let y1 = expect_f64(next_flat_val(items, &mut cur, "Line.y1")?, "Line.y1")?;
                let x2 = expect_f64(next_flat_val(items, &mut cur, "Line.x2")?, "Line.x2")?;
                let y2 = expect_f64(next_flat_val(items, &mut cur, "Line.y2")?, "Line.y2")?;
                let color = unpack_rgba(expect_i64(
                    next_flat_val(items, &mut cur, "Line.c")?,
                    "Line.c",
                )?);
                let line_width = expect_f64(next_flat_val(items, &mut cur, "Line.lw")?, "Line.lw")?;
                commands.push(DrawCmd::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    color,
                    line_width,
                });
            }
            5 => {
                let x = expect_f64(next_flat_val(items, &mut cur, "TextRun.x")?, "TextRun.x")?;
                let y = expect_f64(next_flat_val(items, &mut cur, "TextRun.y")?, "TextRun.y")?;
                let font_size_pt =
                    expect_f64(next_flat_val(items, &mut cur, "TextRun.sz")?, "TextRun.sz")?;
                let color = unpack_rgba(expect_i64(
                    next_flat_val(items, &mut cur, "TextRun.c")?,
                    "TextRun.c",
                )?);
                let flags = expect_i64(
                    next_flat_val(items, &mut cur, "TextRun.flags")?,
                    "TextRun.flags",
                )?;
                let text = expect_str(
                    next_flat_val(items, &mut cur, "TextRun.text")?,
                    "TextRun.text",
                )?;
                let font_family = expect_str(
                    next_flat_val(items, &mut cur, "TextRun.fam")?,
                    "TextRun.fam",
                )?;
                last_fam.clone_from(&font_family);
                commands.push(DrawCmd::TextRun {
                    x,
                    y,
                    text,
                    font_family,
                    font_size_pt,
                    color,
                    bold: (flags & 1) != 0,
                    italic: (flags & 2) != 0,
                    underline: (flags & 4) != 0,
                });
            }
            7 => {
                let x = expect_f64(
                    next_flat_val(items, &mut cur, "TextRunCached.x")?,
                    "TextRunCached.x",
                )?;
                let y = expect_f64(
                    next_flat_val(items, &mut cur, "TextRunCached.y")?,
                    "TextRunCached.y",
                )?;
                let font_size_pt = expect_f64(
                    next_flat_val(items, &mut cur, "TextRunCached.sz")?,
                    "TextRunCached.sz",
                )?;
                let color = unpack_rgba(expect_i64(
                    next_flat_val(items, &mut cur, "TextRunCached.c")?,
                    "TextRunCached.c",
                )?);
                let flags = expect_i64(
                    next_flat_val(items, &mut cur, "TextRunCached.flags")?,
                    "TextRunCached.flags",
                )?;
                let text = expect_str(
                    next_flat_val(items, &mut cur, "TextRunCached.text")?,
                    "TextRunCached.text",
                )?;
                commands.push(DrawCmd::TextRun {
                    x,
                    y,
                    text,
                    font_family: last_fam.clone(),
                    font_size_pt,
                    color,
                    bold: (flags & 1) != 0,
                    italic: (flags & 2) != 0,
                    underline: (flags & 4) != 0,
                });
            }
            6 => {
                let x = expect_f64(
                    next_flat_val(items, &mut cur, "CursorCaret.x")?,
                    "CursorCaret.x",
                )?;
                let y = expect_f64(
                    next_flat_val(items, &mut cur, "CursorCaret.y")?,
                    "CursorCaret.y",
                )?;
                let w = expect_f64(
                    next_flat_val(items, &mut cur, "CursorCaret.w")?,
                    "CursorCaret.w",
                )?;
                let h = expect_f64(
                    next_flat_val(items, &mut cur, "CursorCaret.h")?,
                    "CursorCaret.h",
                )?;
                let shape_code = expect_i64(
                    next_flat_val(items, &mut cur, "CursorCaret.shape")?,
                    "CursorCaret.shape",
                )?;
                let color = unpack_rgba(expect_i64(
                    next_flat_val(items, &mut cur, "CursorCaret.c")?,
                    "CursorCaret.c",
                )?);
                commands.push(DrawCmd::CursorCaret {
                    x,
                    y,
                    w,
                    h,
                    shape: CaretShape::from_code(shape_code),
                    color,
                });
            }
            other => {
                return Err(format!(
                    "unknown flat DrawCmd tag {other} at index {}",
                    cur - 1
                ));
            }
        }
    }

    Ok(RenderFrame {
        generation: 0,
        window: WindowConfig {
            title,
            width,
            height,
            background: bg_rgba,
            pointer: PointerStyle::from_code(pointer_code),
        },
        commands,
    })
}

#[inline]
fn read_u8(buf: &[u8], cur: &mut usize, ctx: &str) -> Result<u8, String> {
    let b = *buf
        .get(*cur)
        .ok_or_else(|| format!("unexpected EOF reading u8 for {ctx} at offset {}", *cur))?;
    *cur += 1;
    Ok(b)
}

#[inline]
fn read_u32_le(buf: &[u8], cur: &mut usize, ctx: &str) -> Result<u32, String> {
    let end = cur
        .checked_add(4)
        .ok_or_else(|| format!("offset overflow reading u32 for {ctx}"))?;
    let slice = buf
        .get(*cur..end)
        .ok_or_else(|| format!("unexpected EOF reading u32 for {ctx} at offset {}", *cur))?;
    *cur = end;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[inline]
fn read_f64_le(buf: &[u8], cur: &mut usize, ctx: &str) -> Result<f64, String> {
    let end = cur
        .checked_add(8)
        .ok_or_else(|| format!("offset overflow reading f64 for {ctx}"))?;
    let slice = buf
        .get(*cur..end)
        .ok_or_else(|| format!("unexpected EOF reading f64 for {ctx} at offset {}", *cur))?;
    *cur = end;
    let bits = u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]);
    Ok(f64::from_bits(bits))
}

#[inline]
fn read_utf8_str(buf: &[u8], cur: &mut usize, ctx: &str) -> Result<String, String> {
    let byte_len = read_u32_le(buf, cur, ctx)? as usize;
    let end = cur
        .checked_add(byte_len)
        .ok_or_else(|| format!("offset overflow reading string for {ctx}"))?;
    let slice = buf
        .get(*cur..end)
        .ok_or_else(|| format!("unexpected EOF reading string for {ctx} at offset {}", *cur))?;
    *cur = end;
    std::str::from_utf8(slice)
        .map(str::to_owned)
        .map_err(|e| format!("invalid UTF-8 in {ctx}: {e}"))
}

/// Parse `"ui_commit_frame_bytes"` arguments `[title, width, height, bg_rgba, pointer_code, bytes]`
/// into a [`RenderFrame`] without per-command GC `List` or `Vec<NativeValue>` allocations.
pub fn parse_commit_frame_bytes_args(args: &[NativeValue]) -> Result<RenderFrame, String> {
    if args.len() != 6 {
        return Err(format!(
            "ui_commit_frame_bytes expects 6 arguments, got {}",
            args.len()
        ));
    }
    let title = expect_str(&args[0], "ui_commit_frame_bytes.title")?;
    let raw_width = expect_i64(&args[1], "ui_commit_frame_bytes.width")?;
    let raw_height = expect_i64(&args[2], "ui_commit_frame_bytes.height")?;
    let bg_rgba = unpack_rgba(expect_i64(&args[3], "ui_commit_frame_bytes.bg_rgba")?);
    let pointer_code = expect_i64(&args[4], "ui_commit_frame_bytes.pointer_code")?;
    let raw_bytes = match &args[5] {
        NativeValue::Bytes(b) => b.as_slice(),
        other => {
            return Err(format!(
                "expected Bytes for ui_commit_frame_bytes.wire_bytes, got {other:?}"
            ));
        }
    };

    let width = raw_width.clamp(1, i64::from(i32::MAX)) as i32;
    let height = raw_height.clamp(1, i64::from(i32::MAX)) as i32;

    let mut commands = Vec::with_capacity(raw_bytes.len() / 36);
    let mut cur = 0usize;
    let mut last_fam = String::new();

    while cur < raw_bytes.len() {
        let tag = read_u8(raw_bytes, &mut cur, "DrawCmd.tag")?;
        match tag {
            0 => {
                let x = read_f64_le(raw_bytes, &mut cur, "ClipPush.x")?;
                let y = read_f64_le(raw_bytes, &mut cur, "ClipPush.y")?;
                let w = read_f64_le(raw_bytes, &mut cur, "ClipPush.w")?;
                let h = read_f64_le(raw_bytes, &mut cur, "ClipPush.h")?;
                commands.push(DrawCmd::ClipPush { x, y, w, h });
            }
            1 => commands.push(DrawCmd::ClipPop),
            2 => {
                let x = read_f64_le(raw_bytes, &mut cur, "FillRect.x")?;
                let y = read_f64_le(raw_bytes, &mut cur, "FillRect.y")?;
                let w = read_f64_le(raw_bytes, &mut cur, "FillRect.w")?;
                let h = read_f64_le(raw_bytes, &mut cur, "FillRect.h")?;
                let color = ColorRgba::from_u32(read_u32_le(raw_bytes, &mut cur, "FillRect.c")?);
                let radius = read_f64_le(raw_bytes, &mut cur, "FillRect.r")?;
                commands.push(DrawCmd::FillRect {
                    x,
                    y,
                    w,
                    h,
                    color,
                    radius,
                });
            }
            3 => {
                let x = read_f64_le(raw_bytes, &mut cur, "StrokeRect.x")?;
                let y = read_f64_le(raw_bytes, &mut cur, "StrokeRect.y")?;
                let w = read_f64_le(raw_bytes, &mut cur, "StrokeRect.w")?;
                let h = read_f64_le(raw_bytes, &mut cur, "StrokeRect.h")?;
                let color = ColorRgba::from_u32(read_u32_le(raw_bytes, &mut cur, "StrokeRect.c")?);
                let line_width = read_f64_le(raw_bytes, &mut cur, "StrokeRect.lw")?;
                let radius = read_f64_le(raw_bytes, &mut cur, "StrokeRect.r")?;
                commands.push(DrawCmd::StrokeRect {
                    x,
                    y,
                    w,
                    h,
                    color,
                    line_width,
                    radius,
                });
            }
            4 => {
                let x1 = read_f64_le(raw_bytes, &mut cur, "Line.x1")?;
                let y1 = read_f64_le(raw_bytes, &mut cur, "Line.y1")?;
                let x2 = read_f64_le(raw_bytes, &mut cur, "Line.x2")?;
                let y2 = read_f64_le(raw_bytes, &mut cur, "Line.y2")?;
                let color = ColorRgba::from_u32(read_u32_le(raw_bytes, &mut cur, "Line.c")?);
                let line_width = read_f64_le(raw_bytes, &mut cur, "Line.lw")?;
                commands.push(DrawCmd::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    color,
                    line_width,
                });
            }
            5 => {
                let x = read_f64_le(raw_bytes, &mut cur, "TextRun.x")?;
                let y = read_f64_le(raw_bytes, &mut cur, "TextRun.y")?;
                let font_size_pt = read_f64_le(raw_bytes, &mut cur, "TextRun.sz")?;
                let color = ColorRgba::from_u32(read_u32_le(raw_bytes, &mut cur, "TextRun.c")?);
                let flags = read_u8(raw_bytes, &mut cur, "TextRun.flags")?;
                let text = read_utf8_str(raw_bytes, &mut cur, "TextRun.text")?;
                let font_family = read_utf8_str(raw_bytes, &mut cur, "TextRun.fam")?;
                last_fam.clone_from(&font_family);
                commands.push(DrawCmd::TextRun {
                    x,
                    y,
                    text,
                    font_family,
                    font_size_pt,
                    color,
                    bold: (flags & 1) != 0,
                    italic: (flags & 2) != 0,
                    underline: (flags & 4) != 0,
                });
            }
            7 => {
                let x = read_f64_le(raw_bytes, &mut cur, "TextRunCached.x")?;
                let y = read_f64_le(raw_bytes, &mut cur, "TextRunCached.y")?;
                let font_size_pt = read_f64_le(raw_bytes, &mut cur, "TextRunCached.sz")?;
                let color =
                    ColorRgba::from_u32(read_u32_le(raw_bytes, &mut cur, "TextRunCached.c")?);
                let flags = read_u8(raw_bytes, &mut cur, "TextRunCached.flags")?;
                let text = read_utf8_str(raw_bytes, &mut cur, "TextRunCached.text")?;
                commands.push(DrawCmd::TextRun {
                    x,
                    y,
                    text,
                    font_family: last_fam.clone(),
                    font_size_pt,
                    color,
                    bold: (flags & 1) != 0,
                    italic: (flags & 2) != 0,
                    underline: (flags & 4) != 0,
                });
            }
            6 => {
                let x = read_f64_le(raw_bytes, &mut cur, "CursorCaret.x")?;
                let y = read_f64_le(raw_bytes, &mut cur, "CursorCaret.y")?;
                let w = read_f64_le(raw_bytes, &mut cur, "CursorCaret.w")?;
                let h = read_f64_le(raw_bytes, &mut cur, "CursorCaret.h")?;
                let shape_code = i64::from(read_u8(raw_bytes, &mut cur, "CursorCaret.shape")?);
                let color = ColorRgba::from_u32(read_u32_le(raw_bytes, &mut cur, "CursorCaret.c")?);
                commands.push(DrawCmd::CursorCaret {
                    x,
                    y,
                    w,
                    h,
                    shape: CaretShape::from_code(shape_code),
                    color,
                });
            }
            other => {
                return Err(format!(
                    "unknown binary DrawCmd tag {other} at offset {cur}"
                ));
            }
        }
    }

    Ok(RenderFrame {
        generation: 0,
        window: WindowConfig {
            title,
            width,
            height,
            background: bg_rgba,
            pointer: PointerStyle::from_code(pointer_code),
        },
        commands,
    })
}

pub(crate) fn require_min_len(
    items: &[NativeValue],
    expected: usize,
    context: &str,
) -> Result<(), String> {
    if items.len() < expected {
        Err(format!(
            "{context} requires at least {expected} elements, got {}",
            items.len()
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn expect_list<'a>(
    val: &'a NativeValue,
    context: &str,
) -> Result<&'a [NativeValue], String> {
    match val {
        NativeValue::List(items) => Ok(items.as_slice()),
        other => Err(format!("expected List for {context}, got {other:?}")),
    }
}

pub(crate) fn expect_str(val: &NativeValue, context: &str) -> Result<String, String> {
    match val {
        NativeValue::Str(s) => Ok(s.clone()),
        other => Err(format!("expected String for {context}, got {other:?}")),
    }
}

pub(crate) fn expect_bool(val: &NativeValue, context: &str) -> Result<bool, String> {
    match val {
        NativeValue::Bool(b) => Ok(*b),
        other => Err(format!("expected Bool for {context}, got {other:?}")),
    }
}

pub(crate) fn expect_i64(val: &NativeValue, context: &str) -> Result<i64, String> {
    match val {
        NativeValue::I64(n) => Ok(*n),
        NativeValue::U64(u) => {
            i64::try_from(*u).map_err(|_| format!("u64 overflow for {context}: {u}"))
        }
        other => Err(format!("expected i64 for {context}, got {other:?}")),
    }
}

pub(crate) fn expect_i32(val: &NativeValue, context: &str) -> Result<i32, String> {
    let n = expect_i64(val, context)?;
    i32::try_from(n).map_err(|_| format!("i32 out of range for {context}: {n}"))
}

pub(crate) fn expect_u32(val: &NativeValue, context: &str) -> Result<u32, String> {
    let n = expect_i64(val, context)?;
    u32::try_from(n).map_err(|_| format!("u32 out of range for {context}: {n}"))
}

pub(crate) fn expect_f64(val: &NativeValue, context: &str) -> Result<f64, String> {
    match val {
        NativeValue::F64(f) => Ok(*f),
        NativeValue::I64(i) => Ok(*i as f64),
        NativeValue::U64(u) => Ok(*u as f64),
        other => Err(format!("expected f64 for {context}, got {other:?}")),
    }
}
