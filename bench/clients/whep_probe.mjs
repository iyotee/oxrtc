// WHEP browser probe: headless Edge (or Chromium) viewers of the server under test.
//
// Usage: node whep_probe.mjs <job.json> <out.json>
//
// job.json: { origin, channel, seconds, viewers: [{ label, url, headers, referenceWidth,
//             decode, cycles, holdAfterFirstFrameMs, gapMs, cycleDelaysMs, reconnect }],
//       maxFramesPerViewer }
//
// Per viewer: a recvonly RTCPeerConnection (video + audio), its offer POSTed to
// the WHEP URL once ICE gathering is done (non-trickle, as openIVR's frontend
// does), the answer applied, the track shown in a small <video>. For every
// presented frame (requestVideoFrameCallback) it records the wall time and,
// when `decode` is set, the 12-bit frame code read from the picture
// (clients/framecode.js). getStats() each second. First frame = from the POST
// to the first presented frame. With `cycles`, the viewer connects, waits for
// its first frame, holds `holdAfterFirstFrameMs`, closes, and starts again.
// With `reconnect`, a lost session is replaced as openIVR's live view does.
//
// The page is served at <origin>/__bench_probe__.html by Playwright's router
// (never by the server under test), so the WHEP POST is same-origin. Playwright
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
const frameCodeJs = readFileSync(join(here, 'framecode.js'), 'utf8')

const pageHtml = `<!doctype html><html><head><meta charset="utf-8"><title>bench probe</title>
<style>body{margin:0;background:#000}video{width:160px;height:90px;object-fit:contain}</style>
</head><body></body></html>`

// Runs in the page.
const pageMain = async job => {
  const fc = window.benchFrameCode
  const canvas = document.createElement('canvas')
  const ctx = canvas.getContext('2d', { willReadFrequently: true })
  const clockSkew = () => Date.now() - (performance.timeOrigin + performance.now())
  const result = { clockSkewStartMs: clockSkew(), viewers: [] }
  // Chromium enumerates every interface (loopback included) only for a page
  // granted media capture; otherwise it offers the default route's address
  // alone, which cannot reach a server bound to 127.0.0.1. A fake camera is
  // opened once and stopped: the grant is what counts.
  try {
    const media = await navigator.mediaDevices.getUserMedia({ video: true })
    media.getTracks().forEach(t => t.stop())
    result.mediaGrant = true
  } catch (e) {
    result.mediaGrant = String(e).slice(0, 120)
  }
  const wall = now => performance.timeOrigin + now
  const maxFrames = job.maxFramesPerViewer || 100000

  const gather = pc =>
    new Promise(resolve => {
      if (pc.iceGatheringState === 'complete') return resolve()
      const timer = setTimeout(resolve, 2000)
      pc.addEventListener('icegatheringstatechange', () => {
        if (pc.iceGatheringState === 'complete') {
          clearTimeout(timer)
          resolve()
        }
      })
    })

  // openIVR's RECONNECT_MIN_DELAY / RECONNECT_MAX_DELAY (stores/streaming.ts)
  const RECONNECT_MIN_MS = 1000
  const RECONNECT_MAX_MS = 30000
  // Resolves with the reason once the session is lost, by openIVR's rule:
  // failed or closed at once, disconnected after 5 s.
  const dropped = pc =>
    new Promise(resolve => {
      let timer
      const check = () => {
        const c = pc.connectionState
        const i = pc.iceConnectionState
        clearTimeout(timer)
        if (c === 'failed' || c === 'closed' || i === 'failed') return resolve(`${c}/${i}`)
        if (c === 'disconnected' || i === 'disconnected') {
          timer = setTimeout(() => {
            if (pc.connectionState === 'disconnected' || pc.iceConnectionState === 'disconnected')
              resolve('disconnected 5 s')
          }, 5000)
        }
      }
      pc.addEventListener('connectionstatechange', check)
      pc.addEventListener('iceconnectionstatechange', check)
      check()
    })

  const readCode = (video, referenceWidth) => {
    const vw = video.videoWidth
    if (!vw) return -1
    const band = fc.bandSize(vw, referenceWidth)
    if (canvas.width !== band.width || canvas.height !== band.height) {
      canvas.width = band.width
      canvas.height = band.height
    }
    ctx.drawImage(video, 0, 0, band.width, band.height, 0, 0, band.width, band.height)
    const px = ctx.getImageData(0, 0, band.width, band.height).data
    return fc.decodeRgba(px, band.width, vw, referenceWidth)
  }

  const statsOf = async pc => {
    const report = await pc.getStats()
    const codecs = {}
    const out = []
    report.forEach(s => {
      if (s.type === 'codec') codecs[s.id] = s.mimeType
    })
    report.forEach(s => {
      if (s.type !== 'inbound-rtp') return
      const keep = { wall: wall(performance.now()), kind: s.kind, codec: codecs[s.codecId] }
      for (const k of ['framesReceived', 'framesDecoded', 'framesDropped', 'framesPerSecond', 'freezeCount',
        'totalFreezesDuration', 'jitterBufferDelay', 'jitterBufferEmittedCount', 'totalDecodeTime',
        'frameWidth', 'frameHeight', 'decoderImplementation', 'packetsLost', 'packetsReceived', 'nackCount',
        'pliCount', 'keyFramesDecoded', 'bytesReceived', 'concealedSamples', 'totalSamplesReceived',
        'jitterBufferTargetDelay', 'jitterBufferMinimumDelay', 'estimatedPlayoutTimestamp',
        'removedSamplesForAcceleration', 'insertedSamplesForDeceleration', 'jitter',
        // W3: what decides smoothness under load - keyframe requests of each
        // kind, retransmissions, the spacing of decoded frames (its mean and
        // spread), the decode pipeline's delay, frames assembled from several
        // packets and packets the jitter buffer threw away.
        'firCount', 'totalInterFrameDelay', 'totalSquaredInterFrameDelay', 'retransmittedPacketsReceived',
        'retransmittedBytesReceived', 'totalProcessingDelay', 'framesAssembledFromMultiplePackets',
        'totalAssemblyTime', 'packetsDiscarded', 'pauseCount', 'totalPausesDuration']) {
        if (s[k] !== undefined) keep[k] = s[k]
      }
      out.push(keep)
    })
    return out
  }

  // The beep the synthetic camera sounds at the instant of its flash
  // (cams/media.py): each onset as the viewer hears it, on the page's clock,
  // so that lip sync is read from the picture and the sound themselves and
  // not from the dates a server puts on them (browser.py av_offsets). The
  // track also plays in the muted <video>: Chromium pulls a remote track's
  // sound only for a media element, and Web Audio gets that same playout.
  const listen = (track, record) => {
    if (record.beepContext) return
    try {
      const ctx = new AudioContext()
      record.beepContext = ctx
      record.beeps = []
      const source = ctx.createMediaStreamSource(new MediaStream([track]))
      const node = ctx.createScriptProcessor(256, 1, 1)
      let quiet = 0
      node.onaudioprocess = e => {
        const x = e.inputBuffer.getChannelData(0)
        // The input of a block is the one before its output's playbackTime.
        const start = e.playbackTime - x.length / ctx.sampleRate
        for (let i = 0; i < x.length; i++) {
          if (Math.abs(x[i]) < 0.1) {
            quiet++
            continue
          }
          if (quiet > ctx.sampleRate / 2 && record.beeps.length < 5000) {
            const out = ctx.getOutputTimestamp()
            const at = start + i / ctx.sampleRate
            record.beeps.push(Math.round(wall(out.performanceTime + (at - out.contextTime) * 1000) * 10) / 10)
          }
          quiet = 0
        }
      }
      source.connect(node)
      node.connect(ctx.destination)
    } catch (e) {
      record.beepError = String(e).slice(0, 200)
    }
  }

  const session = async (spec, record, reconnect = false) => {
    const pc = new RTCPeerConnection()
    pc.addTransceiver('video', { direction: 'recvonly' })
    pc.addTransceiver('audio', { direction: 'recvonly' })
    const video = document.createElement('video')
    video.muted = true
    video.autoplay = true
    video.playsInline = true
    document.body.appendChild(video)
    let firstFrame = null
    const frameSeen = new Promise(resolve => {
      const onFrame = (now, meta) => {
        if (firstFrame === null) {
          firstFrame = wall(now)
          resolve()
        }
        if (record.frames.length < maxFrames) {
          const idx = spec.decode ? readCode(video, spec.referenceWidth) : -1
          record.frames.push([Math.round(wall(now) * 10) / 10, idx, meta.presentedFrames])
        }
        if (!record.closed) video.requestVideoFrameCallback(onFrame)
      }
      video.requestVideoFrameCallback(onFrame)
    })
    pc.ontrack = ev => {
      if (!video.srcObject) video.srcObject = ev.streams[0] || new MediaStream([ev.track])
      else video.srcObject.addTrack(ev.track)
      if (ev.track.kind === 'audio' && spec.decode) listen(ev.track, record)
    }
    await pc.setLocalDescription(await pc.createOffer())
    await gather(pc)
    const postAt = wall(performance.now())
    const res = await fetch(spec.url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/sdp', ...(spec.headers || {}) },
      body: pc.localDescription.sdp,
    })
    const answer = await res.text()
    // the first frame, decomposed: answer, ICE connected, first track
    // Candidates are summarized, never recorded: they carry the machine's LAN
    // and public addresses.
    const candidateKinds = sdp =>
      sdp.split(/\r?\n/).filter(l => l.startsWith('a=candidate')).map(l => {
        const f = l.split(' ')
        const addr = f[4] || ''
        const where = addr === '127.0.0.1' || addr === '::1' ? 'loopback' : addr.endsWith('.local') ? 'mdns' : 'other'
        return `${(f[2] || '').toLowerCase()} ${f[7] || ''} ${where}`
      })
    const attempt = { postAt, reconnect, status: res.status, firstFrameMs: null, answerMs: wall(performance.now()) - postAt,
      offerCandidates: candidateKinds(pc.localDescription.sdp), answerCandidates: candidateKinds(answer) }
    pc.addEventListener('iceconnectionstatechange', () => {
      if (pc.iceConnectionState === 'connected' && attempt.iceConnectedMs === undefined)
        attempt.iceConnectedMs = wall(performance.now()) - postAt
    })
    pc.addEventListener('track', () => {
      if (attempt.trackMs === undefined) attempt.trackMs = wall(performance.now()) - postAt
    })
    record.attempts.push(attempt)
    if (!res.ok) {
      attempt.error = answer.slice(0, 200)
      pc.close()
      video.remove()
      return { pc, video, ok: false }
    }
    await pc.setRemoteDescription({ type: 'answer', sdp: answer })
    const timeout = new Promise(resolve => setTimeout(resolve, 30000))
    await Promise.race([frameSeen, timeout])
    if (firstFrame !== null) attempt.firstFrameMs = firstFrame - postAt
    else attempt.error = 'no frame within 30 s'
    return { pc, video, ok: firstFrame !== null }
  }

  const runViewer = async spec => {
    const record = { label: spec.label, attempts: [], frames: [], stats: [], closed: false }
    result.viewers.push(record)
    const cycles = spec.cycles || 0
    try {
      if (cycles > 0) {
        for (let i = 0; i < cycles; i++) {
          // spread over the camera's GOP (browser.py cycle_delays): without it
          // every offer landed at the same phase of the GOP
          const delay = (spec.cycleDelaysMs || [])[i] || 0
          if (delay) await new Promise(r => setTimeout(r, delay))
          const s = await session(spec, record)
          await new Promise(r => setTimeout(r, spec.holdAfterFirstFrameMs || 1000))
          record.closed = true
          s.pc.close()
          s.video.remove()
          record.closed = false
          // cold start (L03): give the server time to drop its idle source
          if (spec.gapMs) await new Promise(r => setTimeout(r, spec.gapMs))
        }
        return
      }
      let s = await session(spec, record)
      if (!s.ok && !spec.reconnect) return
      const timer = setInterval(async () => {
        try {
          if (s.ok) for (const st of await statsOf(s.pc)) record.stats.push(st)
        } catch (e) {
          record.statsError = String(e)
        }
      }, 1000)
      record.stop = () => {
        clearInterval(timer)
        record.closed = true
        s.pc.close()
      }
      if (!spec.reconnect) return
      // openIVR's live view (frontend/src/stores/streaming.ts): a session that
      // fails, closes, or stays disconnected 5 s is replaced after 1 s, 2 s,
      // 4 s ... up to 30 s; the delay resets once a session shows a picture.
      record.reconnects = []
      let attempts = 0
      for (;;) {
        if (s.ok) {
          attempts = 0
          record.reconnects.push({ droppedAt: null, why: await dropped(s.pc) })
          record.reconnects[record.reconnects.length - 1].droppedAt = wall(performance.now())
        }
        if (record.closed) return
        s.pc.close()
        s.video.remove()
        const delay = Math.min(RECONNECT_MIN_MS * 2 ** attempts, RECONNECT_MAX_MS)
        attempts++
        await new Promise(r => setTimeout(r, delay))
        if (record.closed) return
        try {
          s = await session(spec, record, true)
        } catch (e) {
          record.reconnectError = String(e).slice(0, 200)
          s = { pc: { close() {} }, video: { remove() {} }, ok: false }
        }
      }
    } catch (e) {
      record.error = String(e).slice(0, 300)
    }
  }

  window.__benchStop = () => {
    for (const v of result.viewers) if (v.stop) v.stop()
    result.clockSkewEndMs = clockSkew()
    for (const v of result.viewers) {
      delete v.stop
      if (v.beepContext) v.beepContext.close()
      delete v.beepContext
    }
    return result
  }
  await Promise.all(job.viewers.map(runViewer))
  return true
}

const channel = job.channel === 'chromium' ? undefined : job.channel || 'msedge'
const browser = await chromium.launch({
  channel,
  headless: true,
  // The servers' APIs listen on 127.0.0.1 only and every answer offers a
  // 127.0.0.1 candidate. Chromium does not gather loopback candidates by
  // default and hides its host addresses behind mDNS names: these two
  // switches let it offer real host candidates, loopback included.
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
  const consoleErrors = []
  page.on('console', m => {
    if (m.type() === 'error' && consoleErrors.length < 50) consoleErrors.push(m.text().slice(0, 200))
  })
  await page.route('**/__bench_probe__.html', route =>
    route.fulfill({ status: 200, contentType: 'text/html', body: pageHtml })
  )
  await page.goto(`${job.origin}/__bench_probe__.html`)
  await page.addScriptTag({ content: frameCodeJs })
  const launchedAt = Date.now()
  // Connections run in the page; the run lasts `seconds` from launch, whatever
  // the connections took (a stuck viewer is a result, not a hang).
  const connecting = page.evaluate(pageMain, job).catch(e => String(e))
  await Promise.race([connecting, page.waitForTimeout(job.seconds * 1000)])
  const left = job.seconds * 1000 - (Date.now() - launchedAt)
  if (left > 0) await page.waitForTimeout(left)
  const data = await page.evaluate(() => (window.__benchStop ? window.__benchStop() : null))
  output = { ...output, ...data, launchedAt, seconds: job.seconds, channel: channel || 'chromium',
    ua: await page.evaluate(() => navigator.userAgent), consoleErrors }
} catch (e) {
  output.error = String(e).slice(0, 500)
} finally {
  await browser.close()
}
writeFileSync(outPath, JSON.stringify(output))
const frames = (output.viewers || []).reduce((n, v) => n + v.frames.length, 0)
console.log(`viewers ${(output.viewers || []).length} frames ${frames} error ${output.error}`)
