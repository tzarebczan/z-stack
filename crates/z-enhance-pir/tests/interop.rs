mod fixture;
use z_enhance_pir::{ChainAnchor, Session};

#[test]
fn real_v7_server_roundtrip_and_one_use_contexts() {
    let profile = fixture::profile();
    let server = fixture::Fixture::new(&profile);
    let anchor = ChainAnchor {
        height: 3_500_000,
        hash: [0xef; 32],
        ironwood_tree_size: 67,
    };
    let session = Session::open(&server.manifest, &server.material, &anchor, 100).unwrap();
    let first = session.prepare(33, 100).unwrap().body().unwrap();
    assert_ne!(
        &first[116..],
        &session.prepare(33, 100).unwrap().body().unwrap()[116..]
    );
    for position in [0, 1, 32, 33, 66] {
        let mut query = session.prepare(position, 100).unwrap();
        let body = query.body().unwrap();
        assert!(body.len() <= 282740);
        let answer = server.answer(&body);
        assert_eq!(answer.len(), 30836);
        assert_eq!(
            *query.finish(&answer, 100).unwrap().as_bytes(),
            fixture::record(position)
        );
        assert_eq!(
            query.finish(&answer, 100).unwrap_err(),
            "enhance_context_consumed"
        );
    }
    let mut invalid = session.prepare(0, 100).unwrap();
    assert_eq!(
        invalid.finish(&[0], 100).unwrap_err(),
        "enhance_reply_invalid"
    );
    assert_eq!(invalid.body().unwrap_err(), "enhance_context_consumed");
    let mut cancelled = session.prepare(0, 100).unwrap();
    cancelled.cancel();
    assert_eq!(cancelled.body().unwrap_err(), "enhance_context_consumed");
    assert!(session.prepare(67, 100).is_err());
    assert!(session.prepare(0, 130).is_err());
    let mut late = session.prepare(0, 100).unwrap();
    let late_answer = server.answer(&late.body().unwrap());
    assert_eq!(
        late.finish(&late_answer, 130).unwrap_err(),
        "enhance_routing_refresh_required"
    );
    assert_eq!(late.body().unwrap_err(), "enhance_context_consumed");
    let mut switched = session.prepare(0, 100).unwrap();
    let mut reply = server.answer(&switched.body().unwrap());
    reply[84] ^= 1;
    assert_eq!(
        switched.finish(&reply, 100).unwrap_err(),
        "enhance_reply_invalid"
    );
    assert_eq!(switched.body().unwrap_err(), "enhance_context_consumed");
    let wrong_anchor = ChainAnchor {
        hash: [0; 32],
        ..anchor
    };
    assert!(Session::open(&server.manifest, &server.material, &wrong_anchor, 100).is_err());
}
