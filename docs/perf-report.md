# Collect a frame performance report on Windows

1. Open PowerShell in the extracted Egregoria folder. Start the game with profiling enabled.

   ```powershell
   $env:EGREGORIA_FRAME_PROFILE = '1'
   .\egregoria.exe
   ```

   If your build calls the executable `native_app.exe`, use `.\native_app.exe`.

2. Load the city where the slowdown occurs. Reproduce it with your usual camera movement, graphics settings, and simulation speed. To measure the fastest simulation speed, choose **MAX** on the time controls.
3. In the **Frame Profile** window, click **Save JSON report** shortly after the slowdown. Attach the `frame-profile.json` file from the path shown in the window. A standard install writes it under `world` in the game folder. Saving again replaces the previous report.
4. Close the game when finished. To turn profiling off for future launches from that PowerShell window, run `Remove-Item Env:EGREGORIA_FRAME_PROFILE`.

The report keeps up to 600 presented frames and includes your adapter, graphics settings, resolution, save slot ID, timings, and completed simulation steps. GPU timing needs a backend with timestamp queries and nonblocking query resolve. The OpenGL backend and adapters without both capabilities produce CPU reports. You can set the variable to `cpu` to collect CPU timings alone.

## Benchmark the same city without graphics

Close the game and run this from the extracted `egregoria` folder in PowerShell. The Windows package with cities includes `scenarios.exe` and the bundled saves in `world`.

```powershell
.\scenarios.exe bench-slot --root .\world --slot lesce_town --warmup 500 --ticks 3000 > lesce-town-bench.json
```

The benchmark loads prototypes from `base_mod` beside the executable, with the source checkout as a fallback for development builds. Relative `--root` paths resolve from the directory where you launch the command.

The top-level `ticks_per_sec` field reports completed simulation steps per wall-clock second during the 3,000 measured ticks. Divide it by 50 to compare with the game's MAX readout. Use `--slot lesce_osm` if you reproduced the slowdown in the Lesce OpenStreetMap import. The benchmark loads the selected save and runs it in memory. Share the benchmark JSON alongside the frame report for the same city.

## Compare MAX with headless throughput

Let the city run at **MAX** for several seconds before saving the frame report. In PowerShell, load that report and the benchmark output from the same starting save.

```powershell
$report = Get-Content .\world\frame-profile.json -Raw | ConvertFrom-Json
$headless = Get-Content .\lesce-town-bench.json -Raw | ConvertFrom-Json
$max = @($report.frames | Where-Object { $_.requested_warp -eq 10000 -and $null -ne $_.sim_step })
$first = $max[0]
$last = $max[-1]
$seconds = ($last.sim_sample_ms - $first.sim_sample_ms) / 1000
$stepsPerSecond = ($last.sim_step - $first.sim_step) / $seconds
"In-game {0:N0} steps/s, headless {1:N0} steps/s, ratio {2:P1}" -f $stepsPerSecond, $headless.ticks_per_sec, ($stepsPerSecond / $headless.ticks_per_sec)
```

Use a report with one uninterrupted MAX interval and at least two MAX samples. Pauses and city reloads interrupt the measurement. For repeat runs, reload the same save and use the same camera and graphics settings. If you made no city edits, you can benchmark the exact interval by passing the first MAX sample’s `sim_step` to `--warmup` and the difference between the last and first samples to `--ticks`.

## Reading the report

- `cpu_phases` and `gpu_passes` give the column order for each frame's timing arrays. Times are in milliseconds. A missing pass or result is `null`. `gpu_status` explains whole-frame GPU gaps, including busy readback slots and results still pending at export.
- `present_interval_ms` measures wall time between successful present calls and includes skipped redraws. `sim_sample_ms` is the monotonic time when the paired `sim_step` counter was read, relative to the first recorded simulation sample of the session. `sim_steps_per_second` uses changes in those paired samples over intervals of at least 500 ms. A rate of 50 steps/s is 1×.
- `sim_steps` counts the completed steps between presented samples. `sim_read_lease_ms` measures time the client holds an authoritative read lease. Worker wait fields measure time spent waiting for a held read (`sim_worker_read_wait_ms`) or a reserved redraw turn (`sim_worker_redraw_wait_ms`). `sim_worker_max_idle_ms` includes both when the worker is at MAX, plus other worker waits at MAX.
- `sim_worker_snapshot_ms` and `sim_client_snapshot_ms` time active snapshot capture on the worker and client, respectively. These elapsed times exclude admission waits. `sim_actor_scans` counts actor captures, `sim_actor_buffer_grows` counts buffers whose capacity increased during capture, and `sim_actor_pool_bytes` reports the retained actor and reroute vector capacity in bytes.
- `encode_jobs` sums elapsed times for scene encoding jobs, including encoder setup, deferred work, and finalization. Parallel jobs can overlap each other and `ui_build_encode`, so their summed duration can exceed the frame's wall time. `map_mesh` includes terrain packing and uploads, heightmap regeneration commands, road meshes, and lamp preparation. `world_prep` covers the surrounding update work.
- `lamp_update`, `heightmap_resample`, and `heightmap_normals` appear when the renderer rebuilds those resources, such as after loading a city or editing roads or terrain. The main GPU pass includes water draws. PBR stages and UI blur each span their constituent subpasses. Yakui needs optional encoder timestamps and may be absent on some adapters.

## llvmpipe baseline

The release build ran on llvmpipe Vulkan (Mesa 26.0.3, LLVM 21.1.8) in a 1280 × 720 window with VSync enabled. The capture reloads each save for each preset, warms up for 10 frames at MAX, then measures 60 fixed-view frames and 60 frames of panning and zooming. The weak preset disables shadows and SSAO. MSAA is off in both presets. [Capture script](../scripts/shots/frame_profile_probe.json)

| Save | View | Preset | CPU p95 (ms) | Present p99 (ms) | Main GPU median (ms) | Steps/s |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| Lesce OSM | Fixed | Default | 2,809.9 | 5,267.8 | 496.4 | 401.7 |
| Lesce OSM | Fixed | Weak | 1,647.0 | 2,011.8 | 409.7 | 306.0 |
| Lesce OSM | Pan and zoom | Default | 2,366.4 | 2,969.0 | 614.9 | 365.6 |
| Lesce OSM | Pan and zoom | Weak | 1,050.2 | 1,113.0 | 393.1 | 433.1 |
| Showcase City | Fixed | Default | 2,079.4 | 2,343.7 | 469.9 | 1,646.2 |
| Showcase City | Fixed | Weak | 1,017.9 | 1,855.0 | 324.9 | 1,460.5 |
| Showcase City | Pan and zoom | Default | 2,323.4 | 8,495.6 | 428.6 | 1,555.2 |
| Showcase City | Pan and zoom | Weak | 1,547.6 | 2,716.6 | 334.3 | 1,422.3 |

All 480 frames returned GPU timestamps for the passes enabled in their preset. Steps/s uses the change in `sim_step` divided by the change in `sim_sample_ms`, converted to seconds, between the first and last frames of each capture. CPU frame time includes time spent in graphics API calls. These are single-run software-renderer measurements on a shared host.
