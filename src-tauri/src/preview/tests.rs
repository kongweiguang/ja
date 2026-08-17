// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Preview 的无公网攻击矩阵。
//!
//! 测试只验证纯 policy/registry，不访问网络，也不把浏览器自动化误当成
//! Tauri/Wry 真实窗口证据；真实窗口和 capability probing 由集成 owner 补齐。

use super::*;
use std::time::{Duration, Instant};

#[test]
fn accepts_public_private_localhost_and_custom_port() {
    let policy = PreviewPolicy::new().expect("default policy");
    for raw in [
        "https://example.test:9443/a?x=1",
        "http://localhost:5173/",
        "http://127.0.0.1:4321/health",
        "http://192.168.1.20:8080/",
        "http://[::1]:3000/",
        "HTTP://Example.test:8080/",
        "hTtPs://例子.测试/",
    ] {
        assert!(!policy.validate_url(raw).unwrap().as_str().is_empty());
    }
    assert_eq!(
        policy
            .validate_url("HTTP://Example.test:8080/")
            .unwrap()
            .as_str(),
        "http://example.test:8080/"
    );
}

#[test]
fn rejects_scheme_confusion_and_extra_authority_slashes() {
    let policy = PreviewPolicy::new().unwrap();
    for raw in [
        "http:example.com",
        "https:example.com/path",
        "http:///example.com",
        "https:///example.com",
        "http:////example.com",
        "https:////example.com",
        "//example.com/path",
    ] {
        assert!(policy.validate_url(raw).is_err(), "accepted {raw:?}");
    }
}

#[test]
fn rejects_dangerous_schemes_without_network_access() {
    let policy = PreviewPolicy::new().expect("default policy");
    for raw in [
        "file:///C:/secret.txt",
        "javascript:alert(1)",
        "data:text/html,hello",
        "blob:https://example.test/id",
        "tauri://localhost",
        "custom://example.test/path",
    ] {
        let error = policy.validate_url(raw).unwrap_err();
        assert_eq!(error.code(), PreviewErrorCode::SchemeNotAllowed);
    }
}

#[test]
fn deserializing_url_cannot_bypass_policy() {
    for raw in [
        "file:///C:/secret.txt",
        "javascript:alert(1)",
        "data:text/plain,x",
    ] {
        let encoded = serde_json::to_string(raw).unwrap();
        assert!(serde_json::from_str::<PreviewUrl>(&encoded).is_err());
    }
}

#[test]
fn rejects_userinfo_controls_backslash_and_bad_percent_encoding() {
    let policy = PreviewPolicy::new().expect("default policy");
    for raw in [
        "https://user:password@example.test/",
        "https://@example.test/",
        "https://example.test/%0a",
        "https://example.test/%5c",
        "https://example.test/%zz",
        "https://example.test/unterminated%",
        "https://example.test/a\\b",
        "https://example.test/a\n b",
    ] {
        assert!(
            policy.validate_url(raw).is_err(),
            "accepted attack: {raw:?}"
        );
    }
}

#[test]
fn rejects_oversized_url_before_parser_allocation() {
    let policy = PreviewPolicy::new().expect("default policy");
    let raw = format!("https://example.test/{}", "x".repeat(8 * 1024));
    assert_eq!(
        policy.validate_url(&raw).unwrap_err().code(),
        PreviewErrorCode::UrlTooLong
    );
}

#[test]
fn new_window_is_revalidated_and_isolated() {
    let manager = PreviewManager::default_manager().expect("manager");
    let first = manager.open("https://example.test/").expect("first");
    let result = manager
        .navigate(
            first.snapshot.id,
            first.snapshot.generation,
            NavigationSource::NewWindow,
            "http://localhost:8080/",
        )
        .expect("controlled new window");
    let PreviewNavigationResult::Opened { parent, child } = result else {
        panic!("new window was not controlled");
    };
    assert!(parent.generation > first.snapshot.generation);
    assert_ne!(first.snapshot.id, child.snapshot.id);
    assert_ne!(
        first.snapshot.partition.key(),
        child.snapshot.partition.key()
    );
    let dependency = UnwiredPreviewAdapter
        .open_window(&child.snapshot, &child.window)
        .unwrap_err();
    assert_eq!(dependency.kind(), PreviewDependencyKind::ChildWindow);
    assert_eq!(
        dependency.requirement(),
        IntegrationRequirement::ZeroCapabilityChildWindow
    );
}

#[test]
fn new_window_can_be_fail_closed_without_changing_url_policy() {
    let policy = PreviewPolicy::new()
        .unwrap()
        .with_new_window_behavior(NewWindowBehavior::Reject);
    let decision = policy
        .navigation(NavigationSource::NewWindow, "https://example.test/")
        .unwrap();
    assert_eq!(
        decision,
        PreviewNavigationDecision::Reject {
            code: PreviewErrorCode::NewWindowBlocked
        }
    );
}

#[test]
fn adapter_navigation_input_is_policy_checked_and_generation_bound() {
    let manager = PreviewManager::default_manager().unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    let request = manager
        .navigation_request(
            opened.snapshot.id,
            opened.snapshot.generation,
            NavigationSource::User,
            "HTTPS://Example.test/next",
        )
        .unwrap();
    assert_eq!(
        request.candidate_url().as_str(),
        "https://example.test/next"
    );
    assert_eq!(request.from_generation(), opened.snapshot.generation);
    assert!(request.to_generation() > request.from_generation());
    assert_eq!(
        manager
            .navigate_with_adapter(
                opened.snapshot.id,
                opened.snapshot.generation,
                NavigationSource::User,
                "https://example.test/next",
                &UnwiredPreviewAdapter,
            )
            .unwrap_err()
            .code(),
        PreviewErrorCode::DependencyRequest
    );
    assert_eq!(
        manager.snapshot(opened.snapshot.id).unwrap().generation,
        opened.snapshot.generation
    );
}

#[test]
fn permissions_download_certificate_popup_and_drag_drop_are_denied() {
    let policy = PreviewPolicy::new().expect("policy");
    for permission in [
        PreviewPermission::FileChooser,
        PreviewPermission::Media,
        PreviewPermission::Geolocation,
        PreviewPermission::Notification,
        PreviewPermission::ClipboardRead,
        PreviewPermission::ClipboardWrite,
        PreviewPermission::Camera,
        PreviewPermission::Microphone,
        PreviewPermission::Popup,
        PreviewPermission::DragDrop,
    ] {
        assert_eq!(policy.permission(permission), PermissionDecision::Deny);
    }
    assert_eq!(
        policy.download().unwrap_err().code(),
        PreviewErrorCode::DownloadBlocked
    );
    assert_eq!(
        policy.file_chooser().unwrap_err().code(),
        PreviewErrorCode::PermissionDenied
    );
    assert_eq!(
        policy.certificate_error().unwrap_err().code(),
        PreviewErrorCode::CertificateRejected
    );
    assert_eq!(
        policy.popup().unwrap_err().code(),
        PreviewErrorCode::PopupBlocked
    );
    assert_eq!(
        policy.drag_drop().unwrap_err().code(),
        PreviewErrorCode::DragDropBlocked
    );
}

#[test]
fn stale_generation_is_rejected_after_navigation() {
    let manager = PreviewManager::default_manager().expect("manager");
    let opened = manager.open("https://example.test/").expect("open");
    let committed = manager
        .navigate(
            opened.snapshot.id,
            opened.snapshot.generation,
            NavigationSource::User,
            "https://example.test/next",
        )
        .expect("navigate");
    let PreviewNavigationResult::Committed(snapshot) = committed else {
        panic!("expected same-session navigation");
    };
    assert!(snapshot.generation > opened.snapshot.generation);
    assert_eq!(
        manager
            .callback_title(opened.snapshot.id, opened.snapshot.generation, "late")
            .unwrap_err()
            .code(),
        PreviewErrorCode::StaleGeneration
    );
}

#[test]
fn redirect_and_new_window_advance_generation_before_old_callbacks() {
    let manager = PreviewManager::default_manager().unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    let redirect = manager
        .callback_navigation(
            opened.snapshot.id,
            opened.snapshot.generation,
            "https://example.test/redirect",
        )
        .unwrap();
    assert!(redirect.generation > opened.snapshot.generation);
    assert_eq!(
        manager
            .callback_title(opened.snapshot.id, opened.snapshot.generation, "old")
            .unwrap_err()
            .code(),
        PreviewErrorCode::StaleGeneration
    );

    let PreviewNavigationResult::Opened { parent, .. } = manager
        .navigate(
            opened.snapshot.id,
            redirect.generation,
            NavigationSource::NewWindow,
            "https://example.test/new",
        )
        .unwrap()
    else {
        panic!("expected controlled child");
    };
    assert!(parent.generation > redirect.generation);
    assert_eq!(
        manager
            .callback_close(opened.snapshot.id, redirect.generation)
            .unwrap_err()
            .code(),
        PreviewErrorCode::StaleGeneration
    );
}

#[test]
fn callbacks_and_events_are_bounded_and_utf8_safe() {
    let limits = PreviewLimits {
        max_title_bytes: 5,
        max_error_bytes: 5,
        max_event_count: 3,
        max_event_payload_bytes: 512,
        max_event_queue_bytes: 1024,
        ..PreviewLimits::default()
    };
    let manager = PreviewManager::new(PreviewPolicy::with_limits(limits).unwrap()).unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    let title = manager
        .callback_title(opened.snapshot.id, opened.snapshot.generation, "你好世界")
        .unwrap();
    let PreviewEventKind::TitleChanged { title } = title.kind else {
        panic!("title event");
    };
    assert!(title.len() <= 5);
    assert!(std::str::from_utf8(title.as_bytes()).is_ok());
    for _ in 0..8 {
        manager
            .callback_load_error(
                opened.snapshot.id,
                opened.snapshot.generation,
                "remote page error with lots of detail",
            )
            .unwrap();
    }
    let snapshot = manager.snapshot(opened.snapshot.id).unwrap();
    assert!(snapshot.dropped_events > 0);
    assert!(manager.drain_events(opened.snapshot.id, 99).unwrap().len() <= 3);
    manager
        .callback_load_error(opened.snapshot.id, opened.snapshot.generation, "again")
        .unwrap();
}

#[test]
fn close_is_idempotent_and_late_callbacks_fail_closed() {
    let manager = PreviewManager::default_manager().expect("manager");
    let opened = manager.open("https://example.test/").unwrap();
    let first = manager.close(opened.snapshot.id).unwrap();
    let second = manager.close(opened.snapshot.id).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        manager
            .callback_title(opened.snapshot.id, opened.snapshot.generation, "late")
            .unwrap_err()
            .code(),
        PreviewErrorCode::SessionClosed
    );
}

#[test]
fn site_data_clear_uses_partition_and_rejects_stale_or_duplicate_completion() {
    let manager = PreviewManager::default_manager().expect("manager");
    let opened = manager.open("https://example.test/").unwrap();
    let request = manager
        .request_site_data_clear(opened.snapshot.id, opened.snapshot.generation)
        .unwrap();
    assert_eq!(request.partition(), &opened.snapshot.partition);
    manager
        .complete_site_data_clear(SiteDataClearAck::completed(&request), Instant::now())
        .unwrap();
    assert_eq!(
        manager
            .complete_site_data_clear(SiteDataClearAck::completed(&request), Instant::now())
            .unwrap_err()
            .code(),
        PreviewErrorCode::SiteDataClearStale
    );
}

#[test]
fn site_data_completion_from_previous_generation_is_stale() {
    let manager = PreviewManager::default_manager().unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    let request = manager
        .request_site_data_clear(opened.snapshot.id, opened.snapshot.generation)
        .unwrap();
    let committed = manager
        .navigate(
            opened.snapshot.id,
            opened.snapshot.generation,
            NavigationSource::User,
            "https://example.test/next",
        )
        .unwrap();
    let PreviewNavigationResult::Committed(_) = committed else {
        panic!("expected committed navigation");
    };
    assert_eq!(
        manager
            .complete_site_data_clear(SiteDataClearAck::completed(&request), Instant::now())
            .unwrap_err()
            .code(),
        PreviewErrorCode::SiteDataClearStale
    );
}

#[test]
fn unwired_clear_never_reports_completed_and_expired_ack_fails() {
    let manager = PreviewManager::default_manager().unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    assert_eq!(
        manager
            .clear_site_data_with_adapter(
                opened.snapshot.id,
                opened.snapshot.generation,
                &UnwiredPreviewAdapter,
            )
            .unwrap_err()
            .code(),
        PreviewErrorCode::DependencyRequest
    );
    for _ in 0..600 {
        let _ = manager.callback_load_error(
            opened.snapshot.id,
            opened.snapshot.generation,
            "page error",
        );
    }
    assert!(
        manager
            .drain_events(opened.snapshot.id, 1024)
            .unwrap()
            .iter()
            .any(|event| event.kind == PreviewEventKind::SiteDataClearFailed)
    );

    let request = manager
        .request_site_data_clear(opened.snapshot.id, opened.snapshot.generation)
        .unwrap();
    let expired = request.deadline() + Duration::from_secs(1);
    assert_eq!(
        manager
            .complete_site_data_clear(SiteDataClearAck::completed(&request), expired)
            .unwrap_err()
            .code(),
        PreviewErrorCode::SiteDataClearExpired
    );
    let request = manager
        .request_site_data_clear(opened.snapshot.id, opened.snapshot.generation)
        .unwrap();
    assert_eq!(
        manager
            .expire_site_data_clear(request.deadline() + Duration::from_secs(1))
            .unwrap(),
        1
    );
    assert_eq!(
        manager
            .complete_site_data_clear(SiteDataClearAck::completed(&request), Instant::now())
            .unwrap_err()
            .code(),
        PreviewErrorCode::SiteDataClearStale
    );
}

#[test]
fn session_limit_is_atomic() {
    let limits = PreviewLimits {
        max_sessions: 1,
        ..PreviewLimits::default()
    };
    let manager = PreviewManager::new(PreviewPolicy::with_limits(limits).unwrap()).unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    assert_eq!(
        manager
            .open("https://second.example.test/")
            .unwrap_err()
            .code(),
        PreviewErrorCode::SessionLimit
    );
    manager.close(opened.snapshot.id).unwrap();
    manager.open("https://second.example.test/").unwrap();
}

#[test]
fn unwired_host_adapter_returns_dependency_request() {
    let manager = PreviewManager::default_manager().unwrap();
    let opened = manager.open("https://example.test/").unwrap();
    let adapter = UnwiredPreviewAdapter;
    let error = adapter
        .open_window(&opened.snapshot, &opened.window)
        .unwrap_err();
    assert_eq!(error.kind(), PreviewDependencyKind::ChildWindow);
    assert_eq!(
        error.requirement(),
        IntegrationRequirement::ZeroCapabilityChildWindow
    );
}
