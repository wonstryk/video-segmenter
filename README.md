# Video Segmenter (GUI)

One-click app: pick a video (or drag & drop), set the segment length, press **Split video**.
Single .exe, no installer. On first launch it offers to download FFmpeg automatically.

## Get the .exe — no programming needed
1. Create a free GitHub account and a new repository; upload this folder's contents
   (drag the files into the "Add file > Upload files" page, keeping the `.github` folder).
2. Open the **Actions** tab → **Build Windows EXE** → wait ~5 min.
3. Download **VideoSegmenter-windows** from the finished run → unzip → `video-segmenter.exe`.

## Or build it yourself
Install Rust (https://rustup.rs), then double-click `build-windows.bat`.
Result: `target\release\video-segmenter.exe`
