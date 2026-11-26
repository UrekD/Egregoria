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

The report keeps up to 600 presented frames and includes your adapter, graphics settings, resolution, save slot ID, timings, and completed simulation steps. A GPU that lacks timestamp support still produces a CPU report. You can set the variable to `cpu` if you need to collect CPU timings alone.

## Reading the report

- `cpu_phases` and `gpu_passes` give the column order for each frame's timing arrays. Times are in milliseconds. A missing pass or result is `null`. `gpu_status` explains whole-frame GPU gaps, including busy readback slots and results still pending at export.
- `present_interval_ms` measures wall time between successful present calls and includes skipped redraws. It measures application pacing. The simulation rate uses completed steps over wall time, with 50 steps/s equal to 1×.
- The CPU encode span can overlap UI when parallel rendering is enabled. The main GPU pass includes water draws. PBR stages and UI blur each span their constituent subpasses. Yakui needs optional encoder timestamps and may be absent on some adapters.

## llvmpipe baseline

The release build ran on llvmpipe Vulkan (Mesa 26.0.3, LLVM 21.1.8) in a 1280 × 720 window with VSync enabled. The capture reloads each save for each preset, warms up for 10 frames at MAX, then measures 60 fixed-view frames and 60 frames of panning and zooming. The weak preset disables shadows and SSAO. MSAA is off in both presets. [Capture script](../scripts/shots/rusty_skylines_frame_probe.json)

| Save | View | Preset | CPU p95 (ms) | Present p99 (ms) | Main GPU median (ms) | Steps/s |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| Lesce OSM | Fixed | Default | 1,064.0 | 1,224.0 | 191.4 | 732.4 |
| Lesce OSM | Fixed | Weak | 465.5 | 550.4 | 158.5 | 571.8 |
| Lesce OSM | Pan and zoom | Default | 1,088.7 | 1,180.3 | 202.4 | 658.1 |
| Lesce OSM | Pan and zoom | Weak | 511.8 | 529.7 | 192.5 | 676.5 |
| Showcase City | Fixed | Default | 854.7 | 1,139.9 | 186.9 | 2,599.5 |
| Showcase City | Fixed | Weak | 436.7 | 462.7 | 159.9 | 2,031.5 |
| Showcase City | Pan and zoom | Default | 937.8 | 1,194.0 | 181.8 | 2,564.5 |
| Showcase City | Pan and zoom | Weak | 577.8 | 757.3 | 170.7 | 2,229.0 |

All 480 frames returned GPU timestamps for the passes enabled in their preset. Steps/s uses the change in completed steps divided by elapsed wall time over each capture. CPU frame time includes time spent in graphics API calls. These are single-run software-renderer measurements on a shared host.
