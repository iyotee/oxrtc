// The writer's output in a real browser's MSE (headless Edge, through
// Playwright): run by hand, see crates/mp4/README.md. For each case written by
// the `browser_files` test (H.264 with AAC, with Opus, with A-law packed as
// FLAC; one fragment per frame), it asserts that MediaSource.isTypeSupported
// takes the writer's MIME type and codec strings, then appends the init
// segment and each fragment at its own time, as a live WebSocket feed would,
// plays, and asserts that the picture has its size, that time advances, that
// the sound is decoded, and that what is buffered is one range with no gap.
//
//   node crates/mp4/tests/browser/mse.mjs <dir written by browser_files>
//
// Playwright comes from the openIVR frontend's node_modules
// (OXRTC_PLAYWRIGHT_FROM names another package.json); OXRTC_BROWSER_CHANNEL
// picks the browser (msedge by default, chromium, chrome).
import { createRequire } from 'node:module'
import { readFileSync, readdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const from =
  process.env.OXRTC_PLAYWRIGHT_FROM || join(here, '..', '..', '..', '..', '..', 'openIVR', 'frontend', 'package.json')
const require = createRequire(from)
const { chromium } = require('playwright')

const dir = process.argv[2]
if (!dir) {
  console.error('usage: node mse.mjs <dir written by the browser_files test>')
  process.exit(2)
}
const cases = readdirSync(dir)
  .filter((f) => f.endsWith('.json'))
  .map((f) => JSON.parse(readFileSync(join(dir, f), 'utf8')))
if (cases.length === 0) {
  console.error(`no case in ${dir}`)
  process.exit(2)
}

const origin = 'http://oxrtc.test'
const channel = process.env.OXRTC_BROWSER_CHANNEL === 'chromium' ? undefined : process.env.OXRTC_BROWSER_CHANNEL || 'msedge'
const browser = await chromium.launch({ channel, headless: true, args: ['--autoplay-policy=no-user-gesture-required'] })

// Runs in the page: the whole MSE session of one case.
async function session(c) {
  const out = { supported: MediaSource.isTypeSupported(c.mime), codecs: {} }
  const inner = c.mime.match(/codecs="([^"]*)"/)[1].split(',')
  for (const codec of inner) {
    const kind = codec.startsWith('avc1') || codec.startsWith('hvc1') || codec.startsWith('hev1') ? 'video' : 'audio'
    out.codecs[codec] = MediaSource.isTypeSupported(`${kind}/mp4; codecs="${codec}"`)
  }
  const init = new Uint8Array(await (await fetch(`${c.name}.init.mp4`)).arrayBuffer())
  const all = new Uint8Array(await (await fetch(`${c.name}.m4s`)).arrayBuffer())
  const video = document.createElement('video')
  video.muted = true
  document.body.append(video)
  const source = new MediaSource()
  video.src = URL.createObjectURL(source)
  await new Promise((resolve) => source.addEventListener('sourceopen', resolve, { once: true }))
  const buffer = source.addSourceBuffer(c.mime)
  const errors = []
  video.addEventListener('error', () => errors.push(video.error && video.error.message))
  let stalls = 0
  const append = (bytes) =>
    new Promise((resolve, reject) => {
      buffer.addEventListener('updateend', resolve, { once: true })
      buffer.addEventListener('error', () => reject(new Error('appendBuffer failed')), { once: true })
      buffer.appendBuffer(bytes)
    })
  await append(init)
  const first = c.fragments[0].time
  const t0 = performance.now()
  let at = 0
  let started = null
  for (const f of c.fragments) {
    const due = (f.time - first) * 1000 - (performance.now() - t0)
    if (due > 0) await new Promise((resolve) => setTimeout(resolve, due))
    await append(all.subarray(at, at + f.size))
    at += f.size
    if (started === null && f.time - first >= 0.5 && buffer.buffered.length > 0) {
      video.currentTime = buffer.buffered.start(0)
      await video.play()
      video.addEventListener('waiting', () => stalls++)
      started = { media: video.currentTime, wall: performance.now() }
    }
  }
  await new Promise((resolve) => setTimeout(resolve, 300))
  const ranges = []
  for (let i = 0; i < buffer.buffered.length; i++) ranges.push([buffer.buffered.start(i), buffer.buffered.end(i)])
  const quality = video.getVideoPlaybackQuality()
  return {
    ...out,
    width: video.videoWidth,
    height: video.videoHeight,
    played: started ? video.currentTime - started.media : 0,
    wall: started ? (performance.now() - started.wall) / 1000 : 0,
    ranges,
    frames: quality.totalVideoFrames,
    dropped: quality.droppedVideoFrames,
    soundBytes: video.webkitAudioDecodedByteCount,
    stalls,
    errors,
    appended: at === all.length,
  }
}

let failed = 0
for (const c of cases) {
  const page = await browser.newPage()
  await page.route(`${origin}/**`, (route) => {
    const name = new URL(route.request().url()).pathname.slice(1)
    if (name === '') return route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>mse</title><body></body>' })
    return route.fulfill({ contentType: 'application/octet-stream', body: readFileSync(join(dir, name)) })
  })
  await page.goto(`${origin}/`)
  let r
  try {
    r = await page.evaluate(session, c)
  } catch (error) {
    r = { errors: [String(error)] }
  }
  const problems = []
  if (!r.supported) problems.push(`isTypeSupported(${c.mime}) is false`)
  for (const [codec, ok] of Object.entries(r.codecs || {})) if (!ok) problems.push(`${codec} is not supported`)
  if (!(r.width === 640 && r.height === 360)) problems.push(`picture ${r.width}x${r.height}`)
  if (!(r.played > 4)) problems.push(`played ${r.played} s of 5.5`)
  if (!(r.soundBytes > 0)) problems.push('no sound decoded')
  if (!(r.ranges && r.ranges.length === 1)) problems.push(`buffered ranges ${JSON.stringify(r.ranges)}`)
  if (r.errors && r.errors.length) problems.push(`errors ${r.errors.join('; ')}`)
  if (!r.appended) problems.push('not every fragment appended')
  console.log(`${problems.length ? 'FAIL' : 'ok  '} ${c.name} ${c.mime} ${JSON.stringify(r)}`)
  for (const p of problems) console.log(`     ${p}`)
  failed += problems.length ? 1 : 0
  await page.close()
}
console.log(`${cases.length - failed} of ${cases.length} cases played (${channel || 'chromium'} ${browser.version()})`)
await browser.close()
process.exit(failed ? 1 : 0)
