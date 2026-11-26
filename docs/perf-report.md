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

## Reading the report

- `cpu_phases` and `gpu_passes` give the column order for each frame's timing arrays. Times are in milliseconds. A missing pass or result is `null`. `gpu_status` explains whole-frame GPU gaps, including busy readback slots and results still pending at export.
- `present_interval_ms` measures wall time between successful present calls and includes skipped redraws. `sim_sample_ms` is the monotonic time when the paired `sim_step` counter was read, relative to the first recorded simulation sample of the session. `sim_steps_per_second` uses changes in those paired samples over intervals of at least 500 ms. A rate of 50 steps/s is 1×.
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
