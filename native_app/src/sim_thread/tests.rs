use super::*;
use simulation::world_command::WorldCommand;

fn world() -> (Simulation, SeqSchedule) {
    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    (
        Simulation::new_with_options(scenarios::builtin::smoke::OPTIONS),
        Simulation::schedule(),
    )
}

fn receive(driver: &mut SimThread) -> Reply {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reply) = driver.poll_reply() {
            return reply;
        }
        if driver.is_finished() {
            return driver
                .poll_reply()
                .expect("worker stopped without acknowledging request");
        }
        assert!(
            Instant::now() < deadline,
            "worker did not acknowledge request"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn scale_t1_fifo_pause_capture_and_serial_parity() {
    let (sim, schedule) = world();
    let (mut serial, mut serial_schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 7, None);
    let instant = WorldCommands::from(vec![WorldCommand::SetGameTime(prototypes::GameTime::new(
        prototypes::Tick(sim_tick(&driver) + 30),
    ))]);
    serial.apply_commands(instant.as_ref());
    let first = driver.submit(Request::Batch(instant)).ok().unwrap();
    let commands = WorldCommands::from(vec![WorldCommand::SpawnRandomCars { n_cars: 0 }]);
    serial.tick(&mut serial_schedule, commands.as_ref());
    let batch = driver.submit(Request::Batch(commands)).ok().unwrap();
    let fence = driver
        .submit(Request::PauseFence {
            through_batch: Some(batch),
        })
        .ok()
        .unwrap();
    let ReplyKind::Applied(instant) = receive(&mut driver).kind else {
        panic!("missing instant batch")
    };
    assert_eq!(instant.id, first);
    assert!(!instant.advanced);
    let ReplyKind::Applied(applied) = receive(&mut driver).kind else {
        panic!("missing applied batch")
    };
    assert_eq!(applied.id, batch);
    assert!(applied.advanced);
    let reply = receive(&mut driver);
    assert_eq!(reply.id, fence);
    let ReplyKind::Paused(boundary) = reply.kind else {
        panic!("missing fence")
    };
    assert_eq!(boundary.last_batch, Some(batch));
    assert_eq!(boundary.completed_step, 1);
    let revision = crate::session::snapshot_revision();
    assert!(driver
        .submit(Request::Capture {
            boundary: Some(boundary),
            revision,
            permit: None
        })
        .is_ok());
    let ReplyKind::Captured(Ok((snapshot, progress))) = receive(&mut driver).kind else {
        panic!("capture did not return a snapshot");
    };
    assert_eq!(
        progress,
        CityProgress::capture(&driver.view().try_read().unwrap())
    );
    let root = std::env::temp_dir().join(format!("scale-t1-{:x}", revision.get()));
    let slot = common::saveload::SlotId::legacy();
    snapshot.publish(&root, &slot).unwrap();
    let loaded = Simulation::load_from_slot(&root, &slot).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(loaded.hashes(), serial.hashes());
}

#[test]
fn scale_t1_admission_yields_to_both_roles() {
    let admission = Admission::default();
    let reader = admission.read().unwrap();
    assert!(admission.write().is_none());
    drop(reader);
    assert!(admission.read().is_none());
    let writer = admission.write().unwrap();
    assert!(admission.read().is_none());
    drop(writer);
    assert!(admission.write().is_none());
    assert!(admission.read().is_some());
}

fn sim_tick(driver: &SimThread) -> u64 {
    driver.status().unwrap().boundary.tick
}

#[test]
fn scale_t1_credit_limit_does_not_block_stop_at_max() {
    let (sim, schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 1, None);
    let stale = BatchId {
        generation: 0,
        sequence: 1,
    };
    assert!(matches!(
        driver.try_submit(stale, Request::Resume),
        Err(SubmitErrorWithRequest(SubmitError::Stale, Request::Resume))
    ));
    assert!(driver.submit(Request::SetWarp(MAX_WARP)).is_ok());
    receive(&mut driver);
    let commands = WorldCommands::from(vec![WorldCommand::SetGameTime(prototypes::GameTime::new(
        prototypes::Tick(100),
    ))]);
    for _ in 0..BATCH_CREDITS {
        assert!(driver.submit(Request::Batch(commands.clone())).is_ok());
    }
    let Err(SubmitErrorWithRequest(SubmitError::Full, Request::Batch(returned))) =
        driver.submit(Request::Batch(commands.clone()))
    else {
        panic!("the unpolled replies exhausted the batch credits");
    };
    assert_eq!(
        Bincode::encode(&returned).unwrap(),
        Bincode::encode(&commands).unwrap()
    );
    assert!(driver.submit(Request::Stop).is_ok());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !driver.is_finished() {
        assert!(
            Instant::now() < deadline,
            "stop depended on reply consumption"
        );
        thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..BATCH_CREDITS {
        assert!(matches!(
            driver.poll_reply().unwrap().kind,
            ReplyKind::Applied(_)
        ));
    }
    assert!(matches!(
        driver.poll_reply().unwrap().kind,
        ReplyKind::Stopping
    ));
    assert!(matches!(driver.take_owner(), Some(Ok(_))));
}

#[test]
fn scale_t1_live_capture_respects_frame_admission() {
    let (sim, schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 1, None);
    let view = driver.view();
    let lease = view.try_read().unwrap();
    assert!(driver.submit(Request::SetWarp(MAX_WARP)).is_ok());
    receive(&mut driver);
    assert!(driver
        .submit(Request::Capture {
            revision: crate::session::snapshot_revision(),
            boundary: None,
            permit: None,
        })
        .is_ok());
    let deadline = Instant::now() + Duration::from_secs(10);
    while view.admission.state.load(Ordering::Acquire) & INTENT == 0 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(view.try_read().is_none());
    drop(lease);
    assert!(matches!(
        receive(&mut driver).kind,
        ReplyKind::Captured(Ok(_))
    ));
}

#[test]
fn scale_t1_worker_reports_exact_successful_link_between_rejections() {
    let (sim, schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 1, None);
    let pattern = simulation::map::LanePatternBuilder::default().build();
    let projects = [100.0, 100.5, 310.0, 310.5]
        .into_iter()
        .map(|x| simulation::map::MapProject::ground(geom::vec3(x, 100.0, 0.0)))
        .collect();
    let command = WorldCommand::MapMakeMultipleConnections(
        projects,
        vec![
            (0, 1, None, pattern.clone()),
            (0, 2, None, pattern.clone()),
            (2, 3, None, pattern),
        ],
    );
    assert!(driver
        .submit(Request::Batch(WorldCommands::from(vec![command])))
        .is_ok());
    let ReplyKind::Applied(batch) = receive(&mut driver).kind else {
        panic!("missing batch")
    };
    assert_eq!(batch.rejections.len(), 2);
    assert_eq!(batch.rejections[0], batch.rejections[1]);
    assert_eq!(batch.link_outcomes.len(), 3);
    assert_eq!(batch.link_outcomes[1], (0, (1, Ok(()))));
}

#[test]
fn scale_t1_replay_worker_matches_two_thousand_serial_steps() {
    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let mut serial = Simulation::new_with_options(simulation::SimulationOptions {
        terrain_size: 16,
        ..simulation::SimulationOptions::LEGACY
    });
    let mut schedule = Simulation::schedule();
    for step in 0..2_000 {
        if step == 1200 {
            serial.apply_commands([&WorldCommand::SetGameTime(prototypes::GameTime::new(
                prototypes::Tick(20),
            ))]);
        }
        serial.tick(&mut schedule, &[]);
    }
    let target = serial.get_tick() + 500;
    serial.apply_commands([&WorldCommand::SetGameTime(prototypes::GameTime::new(
        prototypes::Tick(target),
    ))]);
    let recording = serial.read::<simulation::Replay>().clone();
    let (playback, replay) = Simulation::try_from_replay(recording).unwrap();
    let mut driver = SimThread::start(playback, Simulation::schedule(), 5, Some(replay));
    let request = driver
        .submit(Request::ReplayControl(ReplayControl::Step(2_000)))
        .ok()
        .unwrap();
    let reply = receive(&mut driver);
    assert_eq!(reply.id, request);
    let ReplyKind::ReplayStepped(boundary) = reply.kind else {
        panic!("replay did not settle")
    };
    assert_eq!(boundary.completed_step, 2_000);
    assert_eq!(boundary.tick, target);
    let view = driver.view();
    assert_eq!(view.try_read().unwrap().hashes(), serial.hashes());
}

#[test]
fn scale_t1_clock_caps_debt_and_forced_steps_pay_it() {
    let now = Instant::now();
    let mut clock = Clock::new(now);
    clock.warp = 1;
    clock.update(now + Duration::from_secs(3));
    assert_eq!(clock.debt, Duration::from_millis(250));
    let mut steps = 0;
    while clock.due() {
        clock.consume_due();
        steps += 1;
    }
    assert_eq!(steps, 12);
    assert_eq!(clock.wait(), Duration::from_millis(10));
    clock.set_warp(0);
    assert_eq!(clock.debt, Duration::ZERO);
    clock.set_warp(MAX_WARP);
    assert!(clock.due());
}

#[test]
fn scale_t1_frame_can_skip_a_held_tick_and_pause_at_its_boundary() {
    let (sim, mut schedule) = world();
    let (started, running) = mpsc::channel();
    let (finish, parked) = mpsc::channel();
    schedule.add_system(Box::new(simulation::utils::scheduler::RunnableFn {
        name: "test tick gate",
        f: move |_: &mut Simulation| {
            started.send(()).unwrap();
            parked.recv_timeout(Duration::from_secs(10)).unwrap();
        },
    }));
    let mut driver = SimThread::start(sim, schedule, 8, None);
    let view = driver.view();
    assert!(driver.submit(Request::Step { count: 1 }).is_ok());
    running.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(view.try_read().is_none());
    let fence = driver
        .submit(Request::PauseFence {
            through_batch: None,
        })
        .ok()
        .unwrap();
    assert!(driver.poll_reply().is_none());
    finish.send(()).unwrap();
    let ReplyKind::Stepped(boundary) = receive(&mut driver).kind else {
        panic!("missing step")
    };
    assert_eq!(boundary.completed_step, 1);
    let reply = receive(&mut driver);
    assert_eq!(reply.id, fence);
    assert!(matches!(reply.kind, ReplyKind::Paused(ack) if ack == boundary));
    assert_eq!(view.try_read().unwrap().get_tick(), boundary.tick);
}

#[test]
fn scale_t1_pause_can_cancel_writer_intent() {
    let admission = Admission::default();
    let lease = admission.read().unwrap();
    assert!(admission.write().is_none());
    drop(lease);
    assert!(admission.read().is_none());
    admission.cancel_write();
    assert!(admission.read().is_some());
}

#[test]
fn scale_t1_released_owner_preserves_world_and_can_only_be_taken_once() {
    let (sim, schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 4, None);
    let before = driver.view();
    let boundary = driver.status().unwrap().boundary;
    assert!(driver.submit(Request::ReleaseOwner).is_ok());
    assert!(
        matches!(receive(&mut driver).kind, ReplyKind::Releasing(released) if released == boundary)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let owner = loop {
        if let Some(owner) = driver.take_owner() {
            break owner.ok().unwrap();
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    };
    assert_eq!(owner.boundary, boundary);
    assert!(Arc::ptr_eq(&owner.world, &before.world));
    assert!(driver.take_owner().is_none());
    let next = SimThread::from_world(owner.world, owner.schedule, 5);
    assert_eq!(next.status().unwrap().boundary.tick, boundary.tick);
}

#[test]
#[ignore = "wall-clock pacing check takes 30 seconds"]
fn scale_t1_realtime_pacing() {
    let (sim, schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 1, None);
    let start = Instant::now();
    assert!(driver.submit(Request::SetWarp(1)).is_ok());
    receive(&mut driver);
    thread::sleep(Duration::from_secs(30).saturating_sub(start.elapsed()));
    assert!(driver
        .submit(Request::PauseFence {
            through_batch: None
        })
        .is_ok());
    let ReplyKind::Paused(boundary) = receive(&mut driver).kind else {
        panic!("missing pause")
    };
    let rate = boundary.completed_step as f64 / start.elapsed().as_secs_f64();
    eprintln!(
        "1× completed {} steps at {rate:.3} steps/s",
        boundary.completed_step
    );
    assert!((49.5..=50.5).contains(&rate));
}

#[test]
fn scale_t1_one_credit_retains_a_batch_and_a_pause_fence() {
    let (sim, schedule) = world();
    let mut driver = SimThread::start(sim, schedule, 3, None);
    driver.batch_credits = 1;
    let oversized = WorldCommands::from(vec![
        WorldCommand::SpawnRandomCars { n_cars: 0 };
        BATCH_COMMANDS + 1
    ]);
    let Err(SubmitErrorWithRequest(SubmitError::Oversized, Request::Batch(returned))) =
        driver.submit(Request::Batch(oversized))
    else {
        panic!("oversized batch was accepted");
    };
    assert_eq!(returned.as_ref().len(), BATCH_COMMANDS + 1);
    let batch = driver
        .submit(Request::Batch(WorldCommands::from(vec![
            WorldCommand::SpawnRandomCars { n_cars: 0 },
        ])))
        .ok()
        .unwrap();
    assert!(matches!(
        driver.submit(Request::SetWarp(1)),
        Err(SubmitErrorWithRequest(
            SubmitError::Full,
            Request::SetWarp(1)
        ))
    ));
    let fence = driver
        .submit(Request::PauseFence {
            through_batch: Some(batch),
        })
        .ok()
        .unwrap();
    assert_eq!(receive(&mut driver).id, batch);
    assert_eq!(receive(&mut driver).id, fence);
}
