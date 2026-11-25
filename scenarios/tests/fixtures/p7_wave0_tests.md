# P7 Wave 0 test inventory

The stock floor is `88c9e1a08178e7de8ab2fb58f4fdd4837575a9c7`, save version 0.10.0. The fixture and workload pins are in [p7_wave0.json](p7_wave0.json). Test names below are reserved for P7 §6. Each pending test needs its production seam or compatible fixture before it can assert its repair oracle.

## Simulation

| Filter | Stock evidence and repair oracle |
| --- | --- |
| `p7_edit_final_locked_cut` | Pin a fully owned centreline whose final far trim or pending shared cut crosses locked land. Cover protected sockets and widening, wet precedence and X1 exact dry/wet paint and erase. Assert the typed denial and equality of world, allocators, terrain support, land/zones, claims, service, dirty state and cash. |
| `p7_edit_direct_guard` | Repeat final-effect denial through direct Map entry points, including unchecked road/junction removal and terrain mutation. Keep trusted bounded import as a positive control. |
| `p7_edit_quote_curve_stale` | Use authored arc length and the exact final effect quote. Cover curved, dry, imported and elevated decks, stale material effects, insufficient funds, both one-way directions and a current no-op. |
| `p7_edit_paused_cuts` | Save while paused before a legacy cut drain. Resume the same inputs and compare support, service and dirty revisions at the same elapsed boundary. |
| `p7_edit_batch_berth` | Submit an upgrade followed by a berth or route consumer. Preserve original indices and read final map geometry in the later command. |
| `p7_edit_priority_only` | Compare parking, geometry and cuts around a priority-only change and assert the authoritative policy revision. |
| `p7_spatial_density` | Exercise 2k, 4k and 8k co-cell mixed retained/stale lots with multi-cell membership and unrelated occupants. Count discovery and nonmatching work separately from writes. Reject tiny budgets before mutation and check scaling by work count. The existing HARDEN1 split-lot test covers cell scans. HARDEN1-REST additionally requires raw split-child projection work with a configured empty sea. |
| `p7_spatial_projection_order` | Compare mixed-kind observable Map::project winners, hit sets, shape and filter ordering with the stock oracle. |
| `p7_spatial_load_bounds` | Require bounded validation before rebuilding spatial indices and keep legacy/import load green. |
| `p7_spatial_allocator` | Compare split, merge, bulldoze and wet cleanup with stock for allocation/reuse, cut and occupancy state, and untouched entries. |
| `p7_junction_mask_stale` | Mask an exact turn and exclude it from routes and live entry with residual speed, including multiple boundaries in one step. Retain the unmasked control and reject stale geometry. |
| `p7_junction_long_nose` | Cover nose and rear behavior for cars, trucks and buses at the physical boundary. |
| `p7_junction_receiving` | **Compiled, ignored, stock red.** The decision-level fixture holds for ordinary driving and advances in panic with the same occupied perpendicular conflict. Extend to two arrivals competing for one receiving interval and a nonconflicting pair admitted together. |
| `p7_junction_same_tick_walk` | Exercise a same-tick pedestrian entry and the road request at the actual scheduler boundary. |
| `p7_junction_rail_decks` | Cover rail/road conflict on the same deck and a separable grade-crossing control. |
| `p7_junction_rear_resume` | Save halfway through a turn. Retain conflict ownership until the qualifying rear clears after load. |
| `p7_junction_aged_stop` | Cover stop/yield/priority rules, aging under continuous priority, no safe gap and panic protection. |
| `p7_junction_auto_transition` | Add an approach and change Auto policy. Transfer valid rules across degree-two, staggered T, four-leg, curved and parallel layouts. |
| `p7_life_retained_terminal` | Hold the same terminal and intent when retained lane geometry shortens, downstream access is full or a width edit leaves the occupant valid. |
| `p7_life_parking_reversal` | Preserve the claim and owner. Reject an occupied unsafe reversal and apply it once clear. |
| `p7_life_berth_leg` | Preserve active riders, the directed berth and both route legs across a staged edit. |
| `p7_life_assets` | Preserve household, utility, inventory and door endpoints plus loaded company and station trips. |
| `p7_life_alpska` | Archive red at lane 868v1. Reproduce ordinary route loss using a same-build reduction, then prove legal egress and arrival of the original car. The landed inactivity test covers collider cleanup. |
| `p7_life_hraska` | Independent archive red at lane 610v1. Apply the same ordinary-transition and original-car-arrival oracle on a separate reduction. |
| `p7_life_deferred` | Force a tiny cold graph or parking budget. Observe deterministic Deferred and continuation while retaining intent, claim and manifest. |
| `p7_life_elapsed_resume` | Shift calendar time and reload before a due retry. Compare elapsed timer and disposition at the same elapsed step. |
| `p7_life_exactly_once` | Count one terminal disposition per parking claim, rider and manifest across retry, reload and cancellation. |
| `p7_life_unresolved` | Use an impossible legal egress. Check that the live trip remains valid and expiry is reported. |
| `p7_diag_exact_cutoff` | The CM0 stock archive includes real exact-cutoff four-category samples. Exercise sampled clear → exact blocked and sampled blocked → exact clear independently in the live observer. |
| `p7_diag_blocker_chain` | Trace all four queue categories and an actual directed movement cycle. Record typed provenance, recycled handles and omissions. Natural Wave 0 probes supply positional cycle candidates. |
| `p7_diag_lawful_wait` | Long red, legal crossing, stop/bus dwell, rolling, unloading and parking must remain lawful controls. The CITYMAP standing-across-phases test is the stock control. |
| `p7_diag_coverage_budget` | Cap visits, notices and backlog age with tiny budgets. Repeat identical authoritative inputs with observer enabled and disabled. |
| `p7_replay` | C1 stock slice green in `land_ownership_c1_keeps_paused_batches_and_water_prepass_inputs` and the scenario sidecar test. Add actual P7 and tiled receipts, original partial link positions and full gameplay replay parity at named elapsed ticks. |

Save at the paused edit, mid-turn, pre-cut drain and pre-retry boundaries. Pin map and allocator state, timers, queues, routes, admission, occupancy and progress, cargo/stock, utilities, demand and budget. Retain old-schema rejection before payload decode and a bounded readable Paris import control.

## Scenario, network and native

| Filter | Wave 0 / follow-up |
| --- | --- |
| `cm0_stock_cases` | Compiled, ignored by default. Supply the verified schema-v2 matrix with `P7_CM0_MATRIX` to check the stock red/green verdicts, actual counts, worst OD delay, exact cutoff queues and spillback. Run the live upgrade when the prepared edit seam lands. |
| `repair_cohort_and_no_loss` | Pure scorer checks both independent cohorts, OD regressions, equal tags and preserved credit. Add populated routes, facilities, physical receipts and an alternate traversal before the matched gameplay gate. |
| `cutoff_and_manifest_credit` | C2 fixed verdict at primary and tail. A later arrival earns only tail credit. Wrong receiver fails, and missing tags or a reduced offer schedule invalidate the comparison. |
| `manifest_admission` | Fingerprint preflight covers diagnostic versus release, missing or failed opening, schema and worker mismatch. Wire staging to the computed fingerprints of exact checked bytes and the integrated source/schema. |
| `snapshot_and_sidecar_parity` | Earlier legacy checkpoint plus post-restore commands and replay batch slice equality. Extend to the physical repair schedule and native staged capture. |
| `worker_and_native_comparator_parity` | Compiled ignored prerequisite compares stock workload projections at 1, 4 and 16 workers. The floor rejects 4 workers. Add normalized comparator parity with Scale's fenced native capture and compare independent repeat, snapshot continuation and sidecar at aligned cutoffs. |
| `p7_ingress` | Loopback stock control rejects an old peer before world transfer and delivers the world to a matching peer. Extend it through actual simulation ingress when the bounded command DTO is wired, including per-frame and aggregate limits. |
| `p7_receipt` | Add the two-frame local rejection followed by remote success after native indexed receipt wiring. Assert that replacement of the session/capture generation isolates both outcomes. |
