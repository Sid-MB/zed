use std::cmp;

use editor::{display_map::ToDisplayPoint, movement, scroll::Autoscroll, DisplayPoint, RowExt};
use gpui::{impl_actions, ViewContext};
use language::{Bias, SelectionGoal};
use serde::Deserialize;
use workspace::Workspace;

use crate::{
    normal::yank::copy_selections_content,
    state::{Mode, Register},
    Vim,
};

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Paste {
    #[serde(default)]
    before: bool,
    #[serde(default)]
    preserve_clipboard: bool,
}

impl_actions!(vim, [Paste]);

pub(crate) fn register(workspace: &mut Workspace, _: &mut ViewContext<Workspace>) {
    workspace.register_action(paste);
}

fn paste(_: &mut Workspace, action: &Paste, cx: &mut ViewContext<Workspace>) {
    Vim::update(cx, |vim, cx| {
        vim.record_current_action(cx);
        vim.store_visual_marks(cx);
        let count = vim.take_count(cx).unwrap_or(1);

        vim.update_active_editor(cx, |vim, editor, cx| {
            let text_layout_details = editor.text_layout_details(cx);
            editor.transact(cx, |editor, cx| {
                editor.set_clip_at_line_ends(false, cx);

                let selected_register = vim.update_state(|state| state.selected_register.take());

                let Some(Register {
                    text,
                    clipboard_selections,
                }) = vim
                    .read_register(selected_register, Some(editor), cx)
                    .filter(|reg| !reg.text.is_empty())
                else {
                    return;
                };
                let clipboard_selections = clipboard_selections
                    .filter(|sel| sel.len() > 1 && vim.state().mode != Mode::VisualLine);

                if !action.preserve_clipboard && vim.state().mode.is_visual() {
                    copy_selections_content(vim, editor, vim.state().mode == Mode::VisualLine, cx);
                }

                let (display_map, current_selections) = editor.selections.all_adjusted_display(cx);

                // unlike zed, if you have a multi-cursor selection from vim block mode,
                // pasting it will paste it on subsequent lines, even if you don't yet
                // have a cursor there.
                let mut selections_to_process = Vec::new();
                let mut i = 0;
                while i < current_selections.len() {
                    selections_to_process
                        .push((current_selections[i].start..current_selections[i].end, true));
                    i += 1;
                }
                if let Some(clipboard_selections) = clipboard_selections.as_ref() {
                    let left = current_selections
                        .iter()
                        .map(|selection| cmp::min(selection.start.column(), selection.end.column()))
                        .min()
                        .unwrap();
                    let mut row = current_selections.last().unwrap().end.row().next_row();
                    while i < clipboard_selections.len() {
                        let cursor =
                            display_map.clip_point(DisplayPoint::new(row, left), Bias::Left);
                        selections_to_process.push((cursor..cursor, false));
                        i += 1;
                        row.0 += 1;
                    }
                }

                let first_selection_indent_column =
                    clipboard_selections.as_ref().and_then(|zed_selections| {
                        zed_selections
                            .first()
                            .map(|selection| selection.first_line_indent)
                    });
                let before = action.before || vim.state().mode == Mode::VisualLine;

                let mut edits = Vec::new();
                let mut new_selections = Vec::new();
                let mut original_indent_columns = Vec::new();
                let mut start_offset = 0;

                for (ix, (selection, preserve)) in selections_to_process.iter().enumerate() {
                    let (mut to_insert, original_indent_column) =
                        if let Some(clipboard_selections) = &clipboard_selections {
                            if let Some(clipboard_selection) = clipboard_selections.get(ix) {
                                let end_offset = start_offset + clipboard_selection.len;
                                let text = text[start_offset..end_offset].to_string();
                                start_offset = end_offset + 1;
                                (text, Some(clipboard_selection.first_line_indent))
                            } else {
                                ("".to_string(), first_selection_indent_column)
                            }
                        } else {
                            (text.to_string(), first_selection_indent_column)
                        };
                    let line_mode = to_insert.ends_with('\n');
                    let is_multiline = to_insert.contains('\n');

                    if line_mode && !before {
                        if selection.is_empty() {
                            to_insert =
                                "\n".to_owned() + &to_insert[..to_insert.len() - "\n".len()];
                        } else {
                            to_insert = "\n".to_owned() + &to_insert;
                        }
                    } else if !line_mode && vim.state().mode == Mode::VisualLine {
                        to_insert = to_insert + "\n";
                    }

                    let display_range = if !selection.is_empty() {
                        selection.start..selection.end
                    } else if line_mode {
                        let point = if before {
                            movement::line_beginning(&display_map, selection.start, false)
                        } else {
                            movement::line_end(&display_map, selection.start, false)
                        };
                        point..point
                    } else {
                        let point = if before {
                            selection.start
                        } else {
                            movement::saturating_right(&display_map, selection.start)
                        };
                        point..point
                    };

                    let point_range = display_range.start.to_point(&display_map)
                        ..display_range.end.to_point(&display_map);
                    let anchor = if is_multiline || vim.state().mode == Mode::VisualLine {
                        display_map.buffer_snapshot.anchor_before(point_range.start)
                    } else {
                        display_map.buffer_snapshot.anchor_after(point_range.end)
                    };

                    if *preserve {
                        new_selections.push((anchor, line_mode, is_multiline));
                    }
                    edits.push((point_range, to_insert.repeat(count)));
                    original_indent_columns.extend(original_indent_column);
                }

                editor.edit_with_block_indent(edits, original_indent_columns, cx);

                // in line_mode vim will insert the new text on the next (or previous if before) line
                // and put the cursor on the first non-blank character of the first inserted line (or at the end if the first line is blank).
                // otherwise vim will insert the next text at (or before) the current cursor position,
                // the cursor will go to the last (or first, if is_multiline) inserted character.
                editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                    s.replace_cursors_with(|map| {
                        let mut cursors = Vec::new();
                        for (anchor, line_mode, is_multiline) in &new_selections {
                            let mut cursor = anchor.to_display_point(map);
                            if *line_mode {
                                if !before {
                                    cursor = movement::down(
                                        map,
                                        cursor,
                                        SelectionGoal::None,
                                        false,
                                        &text_layout_details,
                                    )
                                    .0;
                                }
                                cursor = movement::indented_line_beginning(map, cursor, true);
                            } else if !is_multiline {
                                cursor = movement::saturating_left(map, cursor)
                            }
                            cursors.push(cursor);
                            if vim.state().mode == Mode::VisualBlock {
                                break;
                            }
                        }

                        cursors
                    });
                })
            });
        });
        vim.switch_mode(Mode::Normal, true, cx);
    });
}

#[cfg(test)]
mod test {
    use crate::{
        state::Mode,
        test::{NeovimBackedTestContext, VimTestContext},
        UseSystemClipboard, VimSettings,
    };
    use gpui::ClipboardItem;
    use indoc::indoc;
    use settings::SettingsStore;

    #[gpui::test]
    async fn test_paste(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        // single line
        cx.set_shared_state(indoc! {"
            The quick brown
            fox ˇjumps over
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("v w y").await;
        cx.shared_clipboard().await.assert_eq("jumps o");
        cx.set_shared_state(indoc! {"
            The quick brown
            fox jumps oveˇr
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            fox jumps overjumps ˇo
            the lazy dog"});

        cx.set_shared_state(indoc! {"
            The quick brown
            fox jumps oveˇr
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            fox jumps ovejumps ˇor
            the lazy dog"});

        // line mode
        cx.set_shared_state(indoc! {"
            The quick brown
            fox juˇmps over
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("d d").await;
        cx.shared_clipboard().await.assert_eq("fox jumps over\n");
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            the laˇzy dog"});
        cx.simulate_shared_keystrokes("p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            the lazy dog
            ˇfox jumps over"});
        cx.simulate_shared_keystrokes("k shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            ˇfox jumps over
            the lazy dog
            fox jumps over"});

        // multiline, cursor to first character of pasted text.
        cx.set_shared_state(indoc! {"
            The quick brown
            fox jumps ˇover
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("v j y").await;
        cx.shared_clipboard().await.assert_eq("over\nthe lazy do");

        cx.simulate_shared_keystrokes("p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            fox jumps oˇover
            the lazy dover
            the lazy dog"});
        cx.simulate_shared_keystrokes("u shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            fox jumps ˇover
            the lazy doover
            the lazy dog"});
    }

    #[gpui::test]
    async fn test_yank_system_clipboard_never(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<VimSettings>(cx, |s| {
                s.use_system_clipboard = Some(UseSystemClipboard::Never)
            });
        });

        cx.set_state(
            indoc! {"
                The quick brown
                fox jˇumps over
                the lazy dog"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("v i w y");
        cx.assert_state(
            indoc! {"
                The quick brown
                fox ˇjumps over
                the lazy dog"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("p");
        cx.assert_state(
            indoc! {"
                The quick brown
                fox jjumpˇsumps over
                the lazy dog"},
            Mode::Normal,
        );
        assert_eq!(cx.read_from_clipboard(), None);
    }

    #[gpui::test]
    async fn test_yank_system_clipboard_on_yank(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<VimSettings>(cx, |s| {
                s.use_system_clipboard = Some(UseSystemClipboard::OnYank)
            });
        });

        // copy in visual mode
        cx.set_state(
            indoc! {"
                The quick brown
                fox jˇumps over
                the lazy dog"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("v i w y");
        cx.assert_state(
            indoc! {"
                The quick brown
                fox ˇjumps over
                the lazy dog"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("p");
        cx.assert_state(
            indoc! {"
                The quick brown
                fox jjumpˇsumps over
                the lazy dog"},
            Mode::Normal,
        );
        assert_eq!(
            cx.read_from_clipboard()
                .map(|item| item.text().map(ToOwned::to_owned).unwrap()),
            Some("jumps".into())
        );
        cx.simulate_keystrokes("d d p");
        cx.assert_state(
            indoc! {"
                The quick brown
                the lazy dog
                ˇfox jjumpsumps over"},
            Mode::Normal,
        );
        assert_eq!(
            cx.read_from_clipboard()
                .map(|item| item.text().map(ToOwned::to_owned).unwrap()),
            Some("jumps".into())
        );
        cx.write_to_clipboard(ClipboardItem::new_string("test-copy".to_string()));
        cx.simulate_keystrokes("shift-p");
        cx.assert_state(
            indoc! {"
                The quick brown
                the lazy dog
                test-copˇyfox jjumpsumps over"},
            Mode::Normal,
        );
    }

    #[gpui::test]
    async fn test_paste_visual(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        // copy in visual mode
        cx.set_shared_state(indoc! {"
                The quick brown
                fox jˇumps over
                the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("v i w y").await;
        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                fox ˇjumps over
                the lazy dog"});
        // paste in visual mode
        cx.simulate_shared_keystrokes("w v i w p").await;
        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                fox jumps jumpˇs
                the lazy dog"});
        cx.shared_clipboard().await.assert_eq("over");
        // paste in visual line mode
        cx.simulate_shared_keystrokes("up shift-v shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            ˇover
            fox jumps jumps
            the lazy dog"});
        cx.shared_clipboard().await.assert_eq("over");
        // paste in visual block mode
        cx.simulate_shared_keystrokes("ctrl-v down down p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            oveˇrver
            overox jumps jumps
            overhe lazy dog"});

        // copy in visual line mode
        cx.set_shared_state(indoc! {"
                The quick brown
                fox juˇmps over
                the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("shift-v d").await;
        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                the laˇzy dog"});
        // paste in visual mode
        cx.simulate_shared_keystrokes("v i w p").await;
        cx.shared_state().await.assert_eq(&indoc! {"
                The quick brown
                the•
                ˇfox jumps over
                 dog"});
        cx.shared_clipboard().await.assert_eq("lazy");
        cx.set_shared_state(indoc! {"
            The quick brown
            fox juˇmps over
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("shift-v d").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The quick brown
            the laˇzy dog"});
        // paste in visual line mode
        cx.simulate_shared_keystrokes("k shift-v p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            ˇfox jumps over
            the lazy dog"});
        cx.shared_clipboard().await.assert_eq("The quick brown\n");
    }

    #[gpui::test]
    async fn test_paste_visual_block(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;
        // copy in visual block mode
        cx.set_shared_state(indoc! {"
            The ˇquick brown
            fox jumps over
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("ctrl-v 2 j y").await;
        cx.shared_clipboard().await.assert_eq("q\nj\nl");
        cx.simulate_shared_keystrokes("p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The qˇquick brown
            fox jjumps over
            the llazy dog"});
        cx.simulate_shared_keystrokes("v i w shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The ˇq brown
            fox jjjumps over
            the lllazy dog"});
        cx.simulate_shared_keystrokes("v i w shift-p").await;

        cx.set_shared_state(indoc! {"
            The ˇquick brown
            fox jumps over
            the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("ctrl-v j y").await;
        cx.shared_clipboard().await.assert_eq("q\nj");
        cx.simulate_shared_keystrokes("l ctrl-v 2 j shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            The qˇqick brown
            fox jjmps over
            the lzy dog"});

        cx.simulate_shared_keystrokes("shift-v p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            ˇq
            j
            fox jjmps over
            the lzy dog"});
    }

    #[gpui::test]
    async fn test_paste_indent(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new_typescript(cx).await;

        cx.set_state(
            indoc! {"
            class A {ˇ
            }
        "},
            Mode::Normal,
        );
        cx.simulate_keystrokes("o a ( ) { escape");
        cx.assert_state(
            indoc! {"
            class A {
                a()ˇ{}
            }
            "},
            Mode::Normal,
        );
        // cursor goes to the first non-blank character in the line;
        cx.simulate_keystrokes("y y p");
        cx.assert_state(
            indoc! {"
            class A {
                a(){}
                ˇa(){}
            }
            "},
            Mode::Normal,
        );
        // indentation is preserved when pasting
        cx.simulate_keystrokes("u shift-v up y shift-p");
        cx.assert_state(
            indoc! {"
                ˇclass A {
                    a(){}
                class A {
                    a(){}
                }
                "},
            Mode::Normal,
        );
    }

    #[gpui::test]
    async fn test_paste_count(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        cx.set_shared_state(indoc! {"
            onˇe
            two
            three
        "})
            .await;
        cx.simulate_shared_keystrokes("y y 3 p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            one
            ˇone
            one
            one
            two
            three
        "});

        cx.set_shared_state(indoc! {"
            one
            ˇtwo
            three
        "})
            .await;
        cx.simulate_shared_keystrokes("y $ $ 3 p").await;
        cx.shared_state().await.assert_eq(indoc! {"
            one
            twotwotwotwˇo
            three
        "});
    }

    #[gpui::test]
    async fn test_numbered_registers(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<VimSettings>(cx, |s| {
                s.use_system_clipboard = Some(UseSystemClipboard::Never)
            });
        });

        cx.set_shared_state(indoc! {"
                The quick brown
                fox jˇumps over
                the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("y y \" 0 p").await;
        cx.shared_register('0').await.assert_eq("fox jumps over\n");
        cx.shared_register('"').await.assert_eq("fox jumps over\n");

        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                fox jumps over
                ˇfox jumps over
                the lazy dog"});
        cx.simulate_shared_keystrokes("k k d d").await;
        cx.shared_register('0').await.assert_eq("fox jumps over\n");
        cx.shared_register('1').await.assert_eq("The quick brown\n");
        cx.shared_register('"').await.assert_eq("The quick brown\n");

        cx.simulate_shared_keystrokes("d d shift-g d d").await;
        cx.shared_register('0').await.assert_eq("fox jumps over\n");
        cx.shared_register('3').await.assert_eq("The quick brown\n");
        cx.shared_register('2').await.assert_eq("fox jumps over\n");
        cx.shared_register('1').await.assert_eq("the lazy dog\n");

        cx.shared_state().await.assert_eq(indoc! {"
        ˇfox jumps over"});

        cx.simulate_shared_keystrokes("d d \" 3 p p \" 1 p").await;
        cx.set_shared_state(indoc! {"
                The quick brown
                fox jumps over
                ˇthe lazy dog"})
            .await;
    }

    #[gpui::test]
    async fn test_named_registers(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<VimSettings>(cx, |s| {
                s.use_system_clipboard = Some(UseSystemClipboard::Never)
            });
        });

        cx.set_shared_state(indoc! {"
                The quick brown
                fox jˇumps over
                the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("\" a d a w").await;
        cx.shared_register('a').await.assert_eq("jumps ");
        cx.simulate_shared_keystrokes("\" shift-a d i w").await;
        cx.shared_register('a').await.assert_eq("jumps over");
        cx.shared_register('"').await.assert_eq("jumps over");
        cx.simulate_shared_keystrokes("\" a p").await;
        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                fox jumps oveˇr
                the lazy dog"});
        cx.simulate_shared_keystrokes("\" a d a w").await;
        cx.shared_register('a').await.assert_eq(" over");
    }

    #[gpui::test]
    async fn test_special_registers(cx: &mut gpui::TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<VimSettings>(cx, |s| {
                s.use_system_clipboard = Some(UseSystemClipboard::Never)
            });
        });

        cx.set_shared_state(indoc! {"
                The quick brown
                fox jˇumps over
                the lazy dog"})
            .await;
        cx.simulate_shared_keystrokes("d i w").await;
        cx.shared_register('-').await.assert_eq("jumps");
        cx.simulate_shared_keystrokes("\" _ d d").await;
        cx.shared_register('_').await.assert_eq("");

        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                the ˇlazy dog"});
        cx.simulate_shared_keystrokes("\" \" d ^").await;
        cx.shared_register('0').await.assert_eq("the ");
        cx.shared_register('"').await.assert_eq("the ");

        cx.simulate_shared_keystrokes("^ \" + d $").await;
        cx.shared_clipboard().await.assert_eq("lazy dog");
        cx.shared_register('"').await.assert_eq("lazy dog");

        cx.simulate_shared_keystrokes("/ d o g enter").await;
        cx.shared_register('/').await.assert_eq("dog");
        cx.simulate_shared_keystrokes("\" / shift-p").await;
        cx.shared_state().await.assert_eq(indoc! {"
                The quick brown
                doˇg"});

        // not testing nvim as it doesn't have a filename
        cx.simulate_keystrokes("\" % p");
        cx.assert_state(
            indoc! {"
                    The quick brown
                    dogdir/file.rˇs"},
            Mode::Normal,
        );
    }

    #[gpui::test]
    async fn test_multicursor_paste(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings::<VimSettings>(cx, |s| {
                s.use_system_clipboard = Some(UseSystemClipboard::Never)
            });
        });

        cx.set_state(
            indoc! {"
               ˇfish one
               fish two
               fish red
               fish blue
                "},
            Mode::Normal,
        );
        cx.simulate_keystrokes("4 g l w escape d i w 0 shift-p");
        cx.assert_state(
            indoc! {"
               onˇefish•
               twˇofish•
               reˇdfish•
               bluˇefish•
                "},
            Mode::Normal,
        );
    }
}
