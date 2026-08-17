// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Pure preview tests cover URL policy, session generations and bounded events.

use super::*;

#[test]
fn accepts_http_https_localhost_and_custom_port() {
    let policy = PreviewPolicy::new().expect("policy");
    for raw in [
        "https://example.test:9443/a?x=1",
        "http://localhost:5173/",
        "http://127.0.0.1:4321/health",
        "http://[::1]:3000/",
        "HTTP://Example.test:8080/",
    ] {
        assert!(!policy.validate_url(raw).expect("url").as_str().is_empty());
    }
    assert_eq!(
        policy
            .validate_url("HTTP://Example.test:8080/")
            .expect("normalized")
            .as_str(),
        "http://example.test:8080/"
    );
}

#[test]
fn rejects_dangerous_schemes_and_authority_confusion() {
    let policy = PreviewPolicy::new().expect("policy");
    for raw in [
        "file:///C:/secret.txt",
        "javascript:alert(1)",
        "data:text/html,hello",
        "tauri://localhost",
        "http:///example.com",
        "https:////example.com",
    ] {
        assert!(policy.validate_url(raw).is_err(), "accepted {raw:?}");
    }
    assert_eq!(
        policy
            .validate_url("javascript:alert(1)")
            .unwrap_err()
            .code(),
        PreviewErrorCode::SchemeNotAllowed
    );
}

#[test]
fn rejects_userinfo_controls_backslash_and_bad_percent_encoding() {
    let policy = PreviewPolicy::new().expect("policy");
    for raw in [
        "https://user:password@example.test/",
        "https://@example.test/",
        "https://example.test/%0a",
        "https://example.test/%5c",
        "https://example.test/%zz",
        "https://example.test/unterminated%",
        "https://example.test/a\\b",
    ] {
        assert!(policy.validate_url(raw).is_err(), "accepted {raw:?}");
    }
}

#[test]
fn url_deserialization_cannot_bypass_policy() {
    assert!(serde_json::from_str::<PreviewUrl>(r#""file:///secret""#).is_err());
    assert!(serde_json::from_str::<PreviewUrl>(r#""https://example.test/""#).is_ok());
}

#[test]
fn open_emits_snapshot_and_event() {
    let manager = PreviewManager::default_manager().expect("manager");
    let opened = manager.open("https://example.test/").expect("open");
    assert_eq!(opened.snapshot.status, PreviewSessionStatus::Open);
    assert_eq!(manager.active_count().expect("count"), 1);
    let events = manager.drain_events(opened.snapshot.id, 8).expect("events");
    assert!(matches!(events[0].kind, PreviewEventKind::Opened { .. }));
}

#[test]
fn navigation_advances_generation_and_rejects_stale_callback() {
    let manager = PreviewManager::default_manager().expect("manager");
    let opened = manager.open("https://example.test/").expect("open");
    let committed = manager
        .navigate(
            opened.snapshot.id,
            opened.snapshot.generation,
            NavigationSource::User,
            "http://localhost:8080/",
        )
        .expect("navigate");
    assert!(committed.generation > opened.snapshot.generation);
    assert_eq!(
        manager
            .callback_navigation(
                opened.snapshot.id,
                opened.snapshot.generation,
                "https://example.test/stale",
            )
            .unwrap_err()
            .code(),
        PreviewErrorCode::StaleGeneration
    );
}

#[test]
fn title_and_load_error_callbacks_are_bounded() {
    let manager = PreviewManager::default_manager().expect("manager");
    let opened = manager.open("https://example.test/").expect("open");
    let title = "界".repeat(2000);
    let event = manager
        .callback_title(opened.snapshot.id, opened.snapshot.generation, &title)
        .expect("title");
    let PreviewEventKind::TitleChanged { title } = event.kind else {
        panic!("title event expected");
    };
    assert!(title.len() <= PreviewLimits::default().max_title_bytes);
    let error = manager
        .callback_load_error(
            opened.snapshot.id,
            opened.snapshot.generation,
            &"x".repeat(9000),
        )
        .expect("error");
    assert!(matches!(error.kind, PreviewEventKind::LoadFailed { .. }));
}

#[test]
fn close_is_idempotent_and_shutdown_releases_active_count() {
    let manager = PreviewManager::default_manager().expect("manager");
    let first = manager.open("https://example.test/1").expect("first");
    let second = manager.open("https://example.test/2").expect("second");
    let closed = manager.close(first.snapshot.id).expect("close");
    assert_eq!(closed.status, PreviewSessionStatus::Closed);
    assert_eq!(
        manager.close(first.snapshot.id).expect("repeat").status,
        PreviewSessionStatus::Closed
    );
    manager.shutdown().expect("shutdown");
    assert_eq!(manager.active_count().expect("count"), 0);
    assert_eq!(
        manager
            .snapshot(second.snapshot.id)
            .expect("snapshot")
            .status,
        PreviewSessionStatus::Closed
    );
}

#[test]
fn event_queue_is_bounded_and_drops_old_history() {
    let policy = PreviewPolicy::with_limits(PreviewLimits {
        max_event_count: 2,
        ..PreviewLimits::default()
    })
    .expect("policy");
    let manager = PreviewManager::new(policy).expect("manager");
    let opened = manager.open("https://example.test/").expect("open");
    manager
        .callback_title(opened.snapshot.id, opened.snapshot.generation, "one")
        .expect("title");
    manager
        .callback_title(opened.snapshot.id, opened.snapshot.generation, "two")
        .expect("title");
    assert!(
        manager
            .snapshot(opened.snapshot.id)
            .expect("snapshot")
            .dropped_events
            > 0
    );
}

#[test]
fn session_limit_is_enforced() {
    let policy = PreviewPolicy::with_limits(PreviewLimits {
        max_sessions: 1,
        ..PreviewLimits::default()
    })
    .expect("policy");
    let manager = PreviewManager::new(policy).expect("manager");
    manager.open("https://example.test/").expect("open");
    assert_eq!(
        manager.open("https://example.test/2").unwrap_err().code(),
        PreviewErrorCode::SessionLimit
    );
}
