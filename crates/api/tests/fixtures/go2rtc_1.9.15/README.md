# go2rtc 1.9.15's answers

Recorded from the go2rtc 1.9.15 release binary (Windows, amd64) on free
loopback ports with a configuration of this project's own (F7, 2026-11-24),
by the script `record_go2rtc.py` of the F7 report: one answer per file, the
body as go2rtc wrote it. Nothing here comes from go2rtc's repository.

What was changed after the recording, so that no file names a machine: the
`config_path` of `api.json` (an absolute path of the recording machine is
now `/config/go2rtc.yaml`), and in `streams_idle.json`, `stream_running.json` and
`streams.dot` the FFmpeg executable and the clip's path of the `exec:` source (now `ffmpeg` and
`clip.mkv`).

The streams: `live`, an `exec:` FFmpeg copying an H.264 and Opus clip into go2rtc's RTSP server
and read by one `stream.mp4` reader; `idle`, an RTSP URL with a credential and nothing behind it;
`two`, two sources; `alias: live`.
