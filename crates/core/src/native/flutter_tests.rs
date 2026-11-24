use super::*;
use serde_json::json;

fn request(operation: Operation) -> Request {
    Request {
        generation: "a".repeat(48),
        id: "request-1".into(),
        operation,
    }
}
fn completion(request: &Request) -> Completion {
    Completion {
        generation: request.generation.clone(),
        id: request.id.clone(),
        operation: request.operation.name(),
        logical_us: 0,
        held: true,
        faulted: false,
        advance: None,
        surface: None,
        capture: None,
        not_applied: None,
    }
}
fn observation() -> Observation {
    Observation {
        observation: "o2".into(),
        revision: 7,
        framework_frame: 4,
        logical_us: 0,
        view: None,
        nodes: vec![Node {
            id: "s19".into(),
            label: "Undo".into(),
            tooltip: "".into(),
            value: "".into(),
            scroll_actions: vec![],
            scroll_position: None,
            scroll_min: None,
            scroll_max: None,
            tappable: true,
            editable: false,
            focused: false,
            rect: [323.4, 790.2, 387.4, 838.2],
        }],
    }
}

#[test]
fn wire_requests_round_trip_and_reject_unobserved_input() {
    let value = json!({"generation":"a".repeat(48),"id":"request-1","operation":"tap","observation":"o2","target":"s19"});
    let decoded: Request = serde_json::from_value(value.clone()).unwrap();
    decoded.validate().unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    for field in ["observation", "target"] {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<Request>(missing).is_err());
    }
    let mut raw = value;
    raw["x"] = json!(10);
    assert!(serde_json::from_value::<Request>(raw).is_err());
    let extra = json!({"generation":"a".repeat(48),"id":"read","operation":"read","delta_us":1});
    assert!(serde_json::from_value::<Request>(extra).is_err());
    assert!(
        request(Operation::Tap {
            observation: "o2".into(),
            target: "arbitrary".into()
        })
        .validate()
        .is_err()
    );
    assert!(
        request(Operation::Advance {
            delta_us: MAX_LOGICAL_US + 1
        })
        .validate()
        .is_err()
    );
}

#[test]
fn admission_rejects_stale_or_noninteractive_observed_targets() {
    let req = request(Operation::Tap {
        observation: "o2".into(),
        target: "s19".into(),
    });
    let mut basis = observation();
    req.validate_basis(Some(&basis)).unwrap();
    assert!(req.validate_basis(None).is_err());
    basis.observation = "o3".into();
    assert!(req.validate_basis(Some(&basis)).is_err());
    basis.observation = "o2".into();
    basis.nodes[0].tappable = false;
    assert!(req.validate_basis(Some(&basis)).is_err());
}

#[test]
fn completed_wire_retains_failed_clock_mutation_and_checks_arithmetic() {
    let req = request(Operation::Advance {
        delta_us: 2_000_000,
    });
    let wire = json!({"status":"completed","generation":req.generation,"id":req.id,
        "operation":"advance","logical_us":0,"held":true,"faulted":true,
        "advance":{"before_us":0,"requested_us":2_000_000,"after_us":0,"time_changed":false,
            "callbacks":8,"drain_completed":false,"framework_frame_completed":false,
            "failure":"callback limit","cancelled_on_failure":1}});
    let Response::Completed(mut receipt) = serde_json::from_value(wire).unwrap() else {
        panic!()
    };
    receipt.validate(&req, 0, None).unwrap();
    receipt.logical_us = 1_000_000;
    let a = receipt.advance.as_mut().unwrap();
    a.after_us = 1_000_000;
    a.time_changed = true;
    receipt.validate(&req, 0, None).unwrap();
    receipt.advance.as_mut().unwrap().time_changed = false;
    assert!(receipt.validate(&req, 0, None).is_err());
    receipt.advance.as_mut().unwrap().time_changed = true;
    receipt.advance.as_mut().unwrap().requested_us = 1_000_000;
    assert!(receipt.validate(&req, 0, None).is_err());
}

#[test]
fn successful_advance_requires_the_requested_time_and_completed_frame() {
    let req = request(Operation::Advance { delta_us: 1000 });
    let mut result = completion(&req);
    result.logical_us = 1000;
    result.advance = Some(Advance {
        before_us: 0,
        requested_us: 1000,
        after_us: 1000,
        time_changed: true,
        callbacks: 0,
        drain_completed: true,
        framework_frame_completed: true,
        failure: None,
        cancelled_on_failure: 0,
    });
    result.validate(&req, 0, None).unwrap();
    result.advance.as_mut().unwrap().framework_frame_completed = false;
    assert!(result.validate(&req, 0, None).is_err());
}

#[test]
fn receipts_cannot_change_generation_operation_or_nonadvance_time() {
    let req = request(Operation::Hold {});
    let good = completion(&req);
    good.validate(&req, 0, None).unwrap();
    let mut bad = good.clone();
    bad.generation = "b".repeat(48);
    assert!(bad.validate(&req, 0, None).is_err());
    let mut bad = good.clone();
    bad.operation = OperationName::Tap;
    assert!(bad.validate(&req, 0, None).is_err());
    let mut bad = good;
    bad.logical_us = 1;
    assert!(bad.validate(&req, 0, None).is_err());
}

#[test]
fn capture_keeps_the_exact_observed_basis_and_partial_cannot_claim_png() {
    let req = request(Operation::Capture {
        observation: "o2".into(),
    });
    let basis = observation();
    let mut result = completion(&req);
    result.capture = Some(Capture::Complete {
        surface: basis.clone(),
        error: None,
        png_base64: "transport-validated-separately".into(),
        raster: Raster::FrameworkLayerPng,
    });
    result.validate(&req, 0, Some(&basis)).unwrap();
    let mut changed = basis.clone();
    changed.framework_frame += 1;
    assert!(result.validate(&req, 0, Some(&changed)).is_err());
    assert!(result.validate(&req, 0, None).is_err());
    result.capture = Some(Capture::Partial {
        surface: basis.clone(),
        error: "stale observation".into(),
        raster: Raster::FrameworkLayerPng,
    });
    result.validate(&req, 0, Some(&basis)).unwrap();
    let mut wire = serde_json::to_value(result.capture.unwrap()).unwrap();
    wire["png_base64"] = json!("unexpected");
    assert!(serde_json::from_value::<Capture>(wire).is_err());
}

#[test]
fn ambiguous_semantic_identity_and_invalid_geometry_reject() {
    let mut basis = observation();
    basis.validate().unwrap();
    basis.nodes.push(basis.nodes[0].clone());
    assert!(basis.validate().is_err());
    basis.nodes.pop();
    basis.nodes[0].rect[0] = f64::NAN;
    assert!(basis.validate().is_err());
    basis.nodes[0].rect = [2.0, 0.0, 1.0, 1.0];
    assert!(basis.validate().is_err());
}

#[test]
fn semantic_coordinates_preserve_the_apps_exact_float_value() {
    // This Android viewport coordinate changed by one ULP with the default JSON parser.
    let wire = r#"{"id":"s0","label":"","tooltip":"","value":"","tappable":false,"rect":[0.0,0.0,411.42857142857144,914.2857142857143]}"#;
    let node: Node = serde_json::from_str(wire).unwrap();
    assert_eq!(node.rect[3].to_bits(), 914.2857142857143_f64.to_bits());
    let saved = serde_json::to_string(&node).unwrap();
    assert!(saved.contains("914.2857142857143"));
    assert_eq!(serde_json::from_str::<Node>(&saved).unwrap(), node);
}

#[test]
fn text_edits_require_focus_and_valid_unicode_boundaries() {
    let mut basis = observation();
    basis.nodes[0].editable = true;
    basis.nodes[0].focused = true;
    basis.nodes[0].value = "부산 여행 🌊".into();
    let edit = |start, end| {
        request(Operation::ReplaceText {
            observation: "o2".into(),
            target: "s19".into(),
            start,
            end,
            text: "주말 여행".into(),
        })
    };
    edit(3, 5).validate_basis(Some(&basis)).unwrap();
    let encoded = serde_json::to_vec(&edit(3, 5)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Request>(&encoded).unwrap(),
        edit(3, 5)
    );
    for (start, end) in [(7, 8), (0, 9), (5, 3)] {
        assert!(edit(start, end).validate_basis(Some(&basis)).is_err());
    }
    basis.nodes[0].focused = false;
    assert!(edit(3, 5).validate_basis(Some(&basis)).is_err());
    basis.nodes[0].focused = true;
    basis.nodes[0].editable = false;
    assert!(edit(3, 5).validate_basis(Some(&basis)).is_err());
    basis.nodes[0].editable = true;
    basis.nodes[0].rect = [0.0, 0.0, 0.0, 10.0];
    assert!(edit(3, 5).validate_basis(Some(&basis)).is_err());
}

#[test]
fn view_geometry_rejects_actions_in_platform_obscured_regions() {
    let mut basis = observation();
    basis.view = Some(ViewGeometry {
        width: 400.0,
        height: 900.0,
        insets: [0.0; 4],
    });
    basis.validate().unwrap();
    basis.view.as_mut().unwrap().insets[3] = 200.0;
    assert!(basis.validate().is_err());
    basis.nodes[0].tappable = false;
    basis.validate().unwrap();
    basis.view.as_mut().unwrap().insets[3] = -1.0;
    assert!(basis.validate().is_err());
}

#[test]
fn scroll_requires_the_observed_direction_and_rejects_invalid_metrics() {
    let mut basis = observation();
    basis.nodes[0].tappable = false;
    basis.nodes[0].scroll_actions = vec![ScrollDirection::Up];
    let req = request(Operation::Scroll {
        observation: "o2".into(),
        target: "s19".into(),
        direction: ScrollDirection::Up,
    });
    req.validate_basis(Some(&basis)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Request>(&serde_json::to_vec(&req).unwrap()).unwrap(),
        req
    );
    basis.nodes[0].scroll_actions.clear();
    assert!(req.validate_basis(Some(&basis)).is_err());
    basis.nodes[0].scroll_min = Some(10.0);
    basis.nodes[0].scroll_max = Some(0.0);
    assert!(basis.validate().is_err());
}

#[test]
fn no_effect_completion_is_bound_to_an_observed_operation_and_preserves_time() {
    let basis = observation();
    let req = request(Operation::Capture {
        observation: "o2".into(),
    });
    let mut result = completion(&req);
    result.not_applied = Some(NotApplied::ObservationChanged);
    result.validate(&req, 0, Some(&basis)).unwrap();
    result.logical_us = 1;
    assert!(result.validate(&req, 0, Some(&basis)).is_err());
    let req = request(Operation::Read {});
    let mut result = completion(&req);
    result.not_applied = Some(NotApplied::ObservationChanged);
    assert!(result.validate(&req, 0, Some(&basis)).is_err());
}
