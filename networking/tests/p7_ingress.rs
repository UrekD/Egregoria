use networking::{
    Client, ConnectConf, Frame, PollResult, Server, ServerConfiguration, ServerPollResult,
};
use std::net::{Ipv4Addr, TcpListener};
use std::time::{Duration, Instant};

#[test]
fn p7_ingress_rejects_old_peer_before_world_transfer() {
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let frame = Frame(0);
    let mut server = Server::<u64, u8>::start(ServerConfiguration {
        start_frame: frame,
        period: Duration::from_millis(20),
        port: Some(port),
        virtual_client: None,
        version: "p7-fixture".into(),
        always_run: false,
    })
    .unwrap();
    let connect = |name: &str, version: &str| {
        Client::<u64, u8>::connect(ConnectConf {
            name: name.into(),
            addr: Ipv4Addr::LOCALHOST.into(),
            port: Some(port),
            frame_buffer_advance: 3,
            version: version.into(),
        })
        .unwrap()
    };

    let world = 19;
    let mut old = connect("old", "p6-fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "old peer did not receive a verdict"
        );
        assert!(matches!(
            server.poll(&world, frame, None),
            ServerPollResult::Wait(None)
        ));
        match old.poll(7) {
            PollResult::Disconnect(reason) => {
                assert!(reason.contains("Incompatible versions"), "{reason}");
                break;
            }
            PollResult::Wait(_) => {}
            _ => panic!("old peer reached world transfer or playout"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut matching = connect("matching", "p7-fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "matching peer received no world");
        let _ = server.poll(&world, frame, None);
        match matching.poll(0) {
            PollResult::GameWorld(_, value) => {
                assert_eq!(value, world);
                break;
            }
            PollResult::Wait(_) => {}
            PollResult::Disconnect(reason) => panic!("matching peer rejected: {reason}"),
            PollResult::Input(_) => panic!("matching peer entered playout before world transfer"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
