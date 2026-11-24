use super::tests::app_with_llmsim as base_app_with_llmsim;
use super::workflow::WorkflowPanel;
use super::*;
use tuika::term::testbackend::TestBackend;

async fn app_with_llmsim() -> super::tests::TestApp {
    let mut test = base_app_with_llmsim().await;
    test.app.setup = None;
    test
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

async fn settle(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.poll_workflow();
        if !app.workflow_presentation().is_some_and(|view| view.loading) {
            return;
        }
        assert!(Instant::now() < deadline, "view did not finish loading");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn workflow_palette_dispatches_views_and_keeps_the_unicode_draft() {
    let mut test = app_with_llmsim().await;
    test.app.set_input_text("Investigate Україна 🦀".into());
    test.app
        .handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await;
    for ch in "/context".chars() {
        test.app.handle_key(key(KeyCode::Char(ch))).await;
    }
    let view = test.app.workflow_presentation().unwrap();
    assert_eq!(view.rows.len(), 1);
    assert!(view.rows[0].contains("/context"));
    test.app.handle_key(key(KeyCode::Enter)).await;
    test.app.pump_ui_commands_for_test().await;
    let view = test.app.workflow_presentation().unwrap();
    assert_eq!(view.title, "Context and usage");
    assert!(view.detail.contains("Context occupancy: unknown"));
    assert!(view.detail.contains("Monetary cost: unknown"));
    assert_eq!(test.app.input_text(), "Investigate Україна 🦀");
    test.app.handle_key(key(KeyCode::Esc)).await;
    assert!(test.app.workflow_presentation().is_none());
    assert_eq!(test.app.input_text(), "Investigate Україна 🦀");
}

#[tokio::test]
async fn workflow_changes_refresh_attach_diff_and_preserve_composer_focus() {
    let mut test = app_with_llmsim().await;
    let root = test._workspace.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Test User"]);
    git(root, &["config", "user.email", "test@example.com"]);
    std::fs::write(root.join("code.rs"), "before\n").unwrap();
    git(root, &["add", "code.rs"]);
    git(root, &["commit", "-qm", "baseline"]);
    std::fs::write(root.join("code.rs"), "after\n").unwrap();
    test.app.set_input_text("Review this".into());
    test.app.dispatch_command_for_test("diff").await;
    settle(&mut test.app).await;
    let view = test.app.workflow_presentation().unwrap();
    assert!(view.rows[0].contains("code.rs"));
    assert!(view.detail.contains("-before"));
    assert!(view.detail.contains("+after"));
    test.app.handle_key(key(KeyCode::Char('a'))).await;
    assert!(!test.app.workflow.focused);
    assert_eq!(
        test.app.workflow_presentation().unwrap().focus_hint,
        "Ctrl+X P focus"
    );
    assert!(test.app.input_text().starts_with("Review this"));
    assert!(test.app.pending_pastes[0].1.contains("+after"));
    test.app.set_input_text(String::new());
    for ch in "Explain the token validation change".chars() {
        test.app.handle_key(key(KeyCode::Char(ch))).await;
    }
    assert_eq!(test.app.input_text(), "Explain the token validation change");
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal.draw(|frame| draw(frame, &mut test.app)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!((0..40).any(|row| {
        (0..80)
            .map(|col| buffer[(col, row)].symbol())
            .collect::<String>()
            .contains("Explain the token validation change")
    }));
    test.app
        .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await;
    test.app.handle_key(key(KeyCode::Char('p'))).await;
    test.app.handle_key(key(KeyCode::Tab)).await;
    settle(&mut test.app).await;
    assert!(
        test.app
            .workflow_presentation()
            .unwrap()
            .detail
            .contains("No workspace snapshot")
    );
    test.app.handle_key(key(KeyCode::Esc)).await;
    test.app.dispatch_command_for_test("diff-base --help").await;
    settle(&mut test.app).await;
    assert!(
        test.app
            .workflow_presentation()
            .unwrap()
            .subtitle
            .contains("--help")
    );
}

#[tokio::test]
async fn workflow_attachment_tray_reads_ranges_removes_payload_and_previews_images() {
    let mut test = app_with_llmsim().await;
    std::fs::write(test._workspace.path().join("code.rs"), "one\ntwo\nthree\n").unwrap();
    test.app.set_input_text("Explain".into());
    test.app
        .dispatch_command_for_test("attach code.rs#2-3")
        .await;
    settle(&mut test.app).await;
    assert!(
        test.app
            .workflow_presentation()
            .unwrap()
            .detail
            .contains("two\nthree")
    );
    assert!(!test.app.pending_pastes[0].1.contains("one\n"));
    test.app.handle_key(key(KeyCode::Delete)).await;
    assert!(test.app.pending_pastes.is_empty());
    assert_eq!(test.app.input_text().trim(), "Explain");
    let mut image = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut image),
            image::ImageFormat::Png,
        )
        .unwrap();
    test.app
        .pending_images
        .push(input::image_input::image_part_from_encoded(&image, "image/png").unwrap());
    test.app.open_workflow(WorkflowPanel::Attachments);
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(test.app.workflow.image_preview.is_some());
    test.app.handle_key(key(KeyCode::Delete)).await;
    assert!(test.app.pending_images.is_empty());
}

#[tokio::test]
async fn workflow_palette_collects_arguments_without_overwriting_a_draft() {
    let mut test = app_with_llmsim().await;
    std::fs::write(test._workspace.path().join("code.rs"), "one\ntwo\n").unwrap();
    test.app.set_input_text("Explain this change".into());
    test.app.open_workflow(WorkflowPanel::Actions);
    test.app.handle_paste("/attach".into()).await;
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(
        test.app.workflow_presentation().unwrap().title,
        "Arguments: /attach"
    );
    assert_eq!(test.app.input_text(), "Explain this change");
    test.app.handle_paste("code.rs#2".into()).await;
    test.app.handle_key(key(KeyCode::Enter)).await;
    test.app.pump_ui_commands_for_test().await;
    settle(&mut test.app).await;
    assert!(test.app.input_text().starts_with("Explain this change"));
    assert!(test.app.pending_pastes[0].1.ends_with("two"));
    assert!(!test.app.input_text().contains("/attach"));
}

#[tokio::test]
async fn workflow_timeline_requires_preview_and_confirmation_and_supports_redo() {
    let mut test = app_with_llmsim().await;
    test.app.start_turn("first request".into());
    test.app.pump_turn_until_idle_for_test().await;
    test.app.start_turn("second request".into());
    test.app.pump_turn_until_idle_for_test().await;
    test.app.dispatch_command_for_test("timeline").await;
    assert_eq!(test.app.workflow_presentation().unwrap().rows.len(), 2);
    test.app.handle_key(key(KeyCode::Enter)).await;
    settle(&mut test.app).await;
    assert_eq!(
        test.app.workflow_presentation().unwrap().title,
        "Confirm restore"
    );
    assert!(
        test.app
            .session
            .active_lines()
            .await
            .unwrap()
            .iter()
            .any(|line| line.text.contains("second request"))
    );
    test.app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(
        test.app.workflow_presentation().unwrap().title,
        "Rewind timeline"
    );
    test.app.handle_key(key(KeyCode::Enter)).await;
    settle(&mut test.app).await;
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(
        !test
            .app
            .session
            .active_lines()
            .await
            .unwrap()
            .iter()
            .any(|line| line.text.contains("second request"))
    );
    assert!(test.app.input_text().contains("second request"));
    test.app.dispatch_command_for_test("timeline").await;
    test.app.handle_key(key(KeyCode::Char('r'))).await;
    settle(&mut test.app).await;
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(
        test.app
            .session
            .active_lines()
            .await
            .unwrap()
            .iter()
            .any(|line| line.text.contains("second request"))
    );
}

#[tokio::test]
async fn workflow_failed_turn_offers_recovery_without_losing_the_draft() {
    let mut test = app_with_llmsim().await;
    let (tx, rx) = mpsc::unbounded_channel();
    test.app.rx = Some(rx);
    test.app.busy = true;
    tx.send(TurnEvent::Failed("authentication failed".into()))
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    test.app.run_loop_iteration(&mut terminal).await.unwrap();
    test.app.set_input_text("unsent follow-up".into());
    test.app.dispatch_command_for_test("recover").await;
    let view = test.app.workflow_presentation().unwrap();
    assert!(view.detail.contains("authentication failed"));
    assert!(view.detail.contains("does not blindly replay"));
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(test.app.busy);
    assert_eq!(test.app.input_text(), "unsent follow-up");
    test.app.pump_turn_until_idle_for_test().await;
    assert!(
        test.app.lines.iter().any(|line| line.author == Author::User
            && line.text.contains("Inspect current workspace state"))
    );
}

#[tokio::test]
async fn workflow_search_jumps_and_modal_paste_never_changes_the_draft() {
    let mut test = app_with_llmsim().await;
    test.app.set_render_mode(RenderMode::Fullscreen);
    test.app.lines = vec![
        ChatLine {
            author: Author::User,
            text: "needle earlier".into(),
        },
        ChatLine {
            author: Author::Assistant,
            text: "other".into(),
        },
    ];
    test.app.set_input_text("keep me".into());
    test.app.dispatch_command_for_test("find").await;
    test.app.handle_paste("needle".into()).await;
    assert_eq!(test.app.workflow_presentation().unwrap().rows.len(), 1);
    assert_eq!(test.app.input_text(), "keep me");
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(test.app.workflow_presentation().is_none());
    assert!(!test.app.scroll.is_stuck_to_bottom());
}

#[tokio::test]
async fn workflow_views_render_at_wide_narrow_and_tiny_sizes_without_stealing_drafts() {
    let mut test = app_with_llmsim().await;
    test.app.set_render_mode(RenderMode::Fullscreen);
    for panel in [
        WorkflowPanel::Actions,
        WorkflowPanel::Usage,
        WorkflowPanel::Attachments,
        WorkflowPanel::Timeline,
        WorkflowPanel::Recovery,
        WorkflowPanel::Changes,
    ] {
        test.app.open_workflow(panel);
        for (width, height) in [(140, 40), (80, 24), (30, 8), (4, 3), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut test.app)).unwrap();
            if width >= 30 && height >= 8 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(!text.trim().is_empty(), "{panel:?} {width}x{height}");
            }
        }
    }
}

#[tokio::test]
async fn workflow_approval_shows_scope_and_never_treats_modified_keys_or_paste_as_consent() {
    let mut test = app_with_llmsim().await;
    test.app.set_input_text("unsent draft".into());
    let (gate, receiver) = crate::sandbox_approval::ApprovalGate::channel();
    test.app.sandbox_approval_rx = receiver;
    let answer = tokio::spawn(async move {
        gate.ask(crate::sandbox_approval::ApprovalRequest {
            command: "git push origin codex/example".into(),
            reason: "Publish this branch".into(),
            scope: crate::sandbox_approval::ApprovalScope::FullAccess,
        })
        .await
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
        test.app.poll_sandbox_approval();
        if test.app.pending_sandbox_approval.is_some() {
            break;
        }
    }
    let request = test
        .app
        .pending_sandbox_approval
        .as_ref()
        .and_then(|pending| pending.request.as_ref())
        .unwrap();
    let detail = workflow_presentation::approval_detail(request);
    assert!(detail.contains("git push origin codex/example"));
    assert!(detail.contains("ALL later requests"));
    assert!(detail.contains("outside sandbox"));
    test.app.handle_paste("a".into()).await;
    test.app
        .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
        .await;
    test.app
        .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT))
        .await;
    assert!(!answer.is_finished());
    assert_eq!(test.app.input_text(), "unsent draft");
    test.app.handle_key(key(KeyCode::Char('y'))).await;
    assert_eq!(
        answer.await.unwrap(),
        Some(crate::sandbox_approval::ApprovalDecision::ApproveOnce)
    );
}

#[tokio::test]
async fn workflow_draft_round_trip_preserves_payloads_and_empty_drafts_clear_saved_state() {
    let mut test = app_with_llmsim().await;
    test.app
        .set_input_text("draft [Pasted Content 1500 chars]".into());
    test.app.pending_pastes = vec![(
        "[Pasted Content 1500 chars]".into(),
        "retained payload".into(),
    )];
    test.app.save_draft().unwrap();
    test.app.reset_input();
    test.app.pending_pastes.clear();
    test.app.restore_draft();
    assert!(test.app.input_text().contains("draft"));
    assert_eq!(test.app.pending_pastes[0].1, "retained payload");
    test.app.reset_input();
    test.app.pending_pastes.clear();
    test.app.save_draft().unwrap();
    test.app.restore_draft();
    assert!(test.app.input_text().is_empty());
}

#[tokio::test]
async fn workflow_session_search_matches_titles_and_refuses_a_live_target() {
    let mut test = app_with_llmsim().await;
    let runtime = crate::runtime::build_with_options(
        test._workspace.path().to_path_buf(),
        crate::runtime::ProviderChoice::Sim,
        None,
        test._sessions.path().to_path_buf(),
        test.app.settings.clone(),
        crate::runtime::BuildOptions {
            client_commands: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut other = App::new(runtime, Vec::new());
    other.start_turn("repair authentication".into());
    other.pump_turn_until_idle_for_test().await;
    let id = other.session_id();
    let directory = test._sessions.path().join(id.to_string());
    let mut metadata = crate::runtime::session_log::read_session_workspace_metadata(&directory)
        .unwrap()
        .unwrap();
    metadata.title = Some("Repair sign-in\u{1b}[2J".into());
    metadata.summary = Some("Next: inspect the authentication diff".into());
    crate::runtime::session_log::write_session_workspace(&directory, &metadata).unwrap();
    test.app.set_input_text("unfinished draft".into());
    test.app.dispatch_command_for_test("sessions").await;
    test.app.handle_paste("sign-in".into()).await;
    tokio::time::sleep(Duration::from_millis(220)).await;
    settle(&mut test.app).await;
    let view = test.app.workflow_presentation().unwrap();
    assert_eq!(view.rows.len(), 1);
    assert!(!view.rows[0].contains('\u{1b}'));
    assert!(
        view.detail
            .contains("Next: inspect the authentication diff")
    );
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(!test.app.should_quit);
    assert!(
        test.app
            .workflow_presentation()
            .unwrap()
            .detail
            .contains("Cannot open session")
    );
    assert_eq!(test.app.input_text(), "unfinished draft");
    drop(other);
    test.app.handle_key(key(KeyCode::Enter)).await;
    assert!(test.app.should_quit);
    assert_eq!(test.app.open_session(), Some(id));
    let saved_draft = test
        ._sessions
        .path()
        .join(test.app.session_id().to_string())
        .join("draft.json");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(saved_draft).unwrap()).unwrap();
    assert_eq!(saved["text"], "unfinished draft");
}
