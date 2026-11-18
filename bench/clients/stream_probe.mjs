// One MediaStream per session: what a browser makes of a WHEP answer.
//
// Usage: node stream_probe.mjs <job.json> <out.json>
//
// job.json: { origin, channel, url, headers, seconds, transceivers: ['video', 'audio'], player: 'first' | 'own' }
//
// The page is the player many WHEP and go2rtc clients are: recvonly
// transceivers, the offer POSTed once ICE gathering is done, and a `track`
// handler that does `video.srcObject = event.streams[0]` at EVERY track
// event. That player shows a picture only when the answer names the same
// MediaStream on every media line: with one stream per line, the handler ends
// on the stream of the last track (the sound), and the picture is black.
//
// Recorded: each track event (kind, track id, the ids of event.streams), the
// <video> after `seconds` (videoWidth, the tracks of its srcObject, frames
// presented), every presented frame (when, how long the browser held it, its
// RTP time), getStats() of the inbound RTP at half time and at the end (with
// each track's estimatedPlayoutTimestamp, the sender's clock of what is being
// played: the picture's minus the sound's is how far the sound is behind the
// picture for the viewer), and
// the lines of the answer that name streams and tracks (no candidate, no
// address: those lines are never recorded).
//
// The page is served at <origin>/__stream_probe__.html by Playwright's router
// (never by the server under test), so the POST is same-origin. Playwright
// comes from the openIVR frontend's node_modules (OXRTC_PLAYWRIGHT_FROM).
import { createRequire } from 'node:module'
import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const from = process.env.OXRTC_PLAYWRIGHT_FROM || join(here, '..', '..', '..', 'openIVR', 'frontend', 'package.json')
const require = createRequire(from)
const { chromium } = require('playwright')

const [jobPath, outPath] = process.argv.slice(2)
const job = JSON.parse(readFileSync(jobPath, 'utf8'))

const pageHtml = `<!doctype html><html><head><meta charset="utf-8"><title>stream probe</title>
<style>body{margin:0;background:#000}video{width:320px;height:180px;object-fit:contain}</style>
</head><body></body></html>`

// Runs in the page.
const pageMain = async job => {
  const result = { tracks: [], errors: [] }
  // The grant that makes Chromium offer loopback candidates (whep_probe.mjs).
  try {
    const media = await navigator.mediaDevices.getUserMedia({ video: true })
    media.getTracks().forEach(t => t.stop())
  } catch (e) {
    result.errors.push(`media grant: ${String(e).slice(0, 120)}`)
  }
  const pc = new RTCPeerConnection()
  for (const kind of job.transceivers) pc.addTransceiver(kind, { direction: 'recvonly' })
  const video = document.createElement('video')
  video.muted = true
  video.autoplay = true
  video.playsInline = true
  document.body.appendChild(video)
  let presented = 0
  // Each presented frame: when it is shown, how long the browser held it
  // (from its last packet's arrival to its display: jitter buffer, decoder
  // and the wait for the sound when the two are one stream) and its RTP time.
  const frames = []
  const onFrame = (now, meta) => {
    presented++
    if (meta && meta.receiveTime !== undefined && frames.length < 20000) {
      frames.push([meta.expectedDisplayTime, meta.expectedDisplayTime - meta.receiveTime, meta.rtpTimestamp])
    }
    video.requestVideoFrameCallback(onFrame)
  }
  video.requestVideoFrameCallback(onFrame)
  // The player under test: the first stream of the event, at every event.
  // With `player: 'own'`, the other common player: a stream of the page's
  // own making that every track is added to (openIVR's since 2026-11-17),
  // which shows a picture whatever streams the answer names.
  const own = job.player === 'own' ? new MediaStream() : null
  pc.ontrack = ev => {
    result.tracks.push({ kind: ev.track.kind, track: ev.track.id, streams: ev.streams.map(s => s.id) })
    try {
      if (own) own.addTrack(ev.track)
      video.srcObject = own || ev.streams[0]
    } catch (e) {
      result.errors.push(`srcObject: ${String(e).slice(0, 160)}`)
    }
  }
  await pc.setLocalDescription(await pc.createOffer())
  await new Promise(resolve => {
    if (pc.iceGatheringState === 'complete') return resolve()
    const timer = setTimeout(resolve, 2000)
    pc.addEventListener('icegatheringstatechange', () => {
      if (pc.iceGatheringState === 'complete') {
        clearTimeout(timer)
        resolve()
      }
    })
  })
  const res = await fetch(job.url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/sdp', ...(job.headers || {}) },
    body: pc.localDescription.sdp,
  })
  const answer = await res.text()
  result.status = res.status
  if (!res.ok) {
    result.errors.push(`answer ${res.status}: ${answer.slice(0, 160)}`)
    pc.close()
    return result
  }
  // What names streams and tracks, and each media line's kind and direction.
  // Never a candidate, a connection line or a fingerprint.
  const kept = /^(m=|a=mid:|a=msid|a=ssrc|a=group:|a=(sendonly|recvonly|sendrecv|inactive)$)/
  result.answer = answer.split(/\r?\n/).filter(l => kept.test(l)).map(l => (l.startsWith('m=') ? l.split(' ')[0] : l))
  await pc.setRemoteDescription({ type: 'answer', sdp: answer })
  const stats = async () => {
    const out = []
    const report = await pc.getStats()
    report.forEach(s => {
      if (s.type !== 'inbound-rtp') return
      const keep = { kind: s.kind }
      for (const k of ['packetsReceived', 'jitter', 'jitterBufferDelay', 'jitterBufferTargetDelay',
        'jitterBufferMinimumDelay', 'jitterBufferEmittedCount', 'framesDecoded', 'totalSamplesReceived',
        'concealedSamples', 'removedSamplesForAcceleration', 'insertedSamplesForDeceleration',
        'packetsDiscarded', 'estimatedPlayoutTimestamp', 'silentConcealedSamples', 'concealmentEvents',
        'freezeCount', 'totalFreezesDuration', 'framesDropped']) {
        if (s[k] !== undefined) keep[k] = s[k]
      }
      out.push(keep)
    })
    return out
  }
  // The run in steps of 100 ms for its first 3 s (the start: what the
  // server sends at once, and what the browser's buffers make of it), then of
  // one second: `timeline` holds every reading, dated from the answer.
  const started = performance.now()
  const half = (job.seconds * 1000) / 2
  result.timeline = []
  while (performance.now() - started < job.seconds * 1000) {
    const elapsed = performance.now() - started
    await new Promise(r => setTimeout(r, elapsed < 3000 ? 100 : 1000))
    const reading = await stats()
    result.timeline.push({ ms: Math.round(performance.now() - started), stats: reading })
    if (!result.statsHalf && performance.now() - started >= half) result.statsHalf = reading
    result.statsEnd = reading
  }
  const shown = video.srcObject
  // [ms since the answer, ms held, RTP time] per presented frame.
  result.frames = frames.map(([at, held, rtp]) => [Math.round(at - started), Math.round(held * 10) / 10, rtp])
  result.video = {
    videoWidth: video.videoWidth,
    videoHeight: video.videoHeight,
    readyState: video.readyState,
    framesPresented: presented,
    srcObject: shown ? { id: shown.id, tracks: shown.getTracks().map(t => ({ kind: t.kind, id: t.id })) } : null,
  }
  result.connection = pc.connectionState
  pc.close()
  return result
}

const channel = job.channel === 'chromium' ? undefined : job.channel || 'msedge'
const browser = await chromium.launch({
  channel,
  headless: true,
  // As whep_probe.mjs: real host candidates, loopback included, no prompt.
  args: [
    '--autoplay-policy=no-user-gesture-required',
    '--allow-loopback-in-peer-connection',
    '--disable-features=WebRtcHideLocalIpsWithMdns',
    '--use-fake-ui-for-media-stream',
    '--use-fake-device-for-media-stream',
  ],
})
let output = { error: null }
try {
  const context = await browser.newContext({ viewport: { width: 1280, height: 720 }, permissions: ['camera'] })
  const page = await context.newPage()
  await page.route('**/__stream_probe__.html', route =>
    route.fulfill({ status: 200, contentType: 'text/html', body: pageHtml })
  )
  await page.goto(`${job.origin}/__stream_probe__.html`)
  const data = await page.evaluate(pageMain, job)
  output = { ...output, ...data, channel: channel || 'chromium', ua: await page.evaluate(() => navigator.userAgent) }
} catch (e) {
  output.error = String(e).slice(0, 500)
} finally {
  await browser.close()
}
writeFileSync(outPath, JSON.stringify(output))
console.log(`tracks ${(output.tracks || []).length} videoWidth ${output.video ? output.video.videoWidth : null} error ${output.error}`)
