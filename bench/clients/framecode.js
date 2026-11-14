// The frame-code reader of the browser probe - the JavaScript twin of
// bench/framecode.py (decode_rgba). Both must read the same code from the same
// pixels: tests/test_framecode.py runs this file under Node against the Python
// decoder, so a change to one without the other fails the tests.
//
// A classic script (no module syntax): the probe injects it into the page as
// text, and the test loads it with node:vm.
;(function (root) {
  const BITS = 12
  const SQUARE = 36

  // pixels: RGBA bytes of the top-left band, `width` pixels per row.
  // videoWidth: the decoded picture's width; referenceWidth: the width the code
  // was burnt at (the camera's main stream). Returns -1 when unreadable.
  function decodeRgba(pixels, width, videoWidth, referenceWidth) {
    if (!(videoWidth > 0) || !(referenceWidth > 0) || !(width > 0)) return -1
    const square = (SQUARE * videoWidth) / referenceWidth
    const y = Math.floor(0.5 * square)
    let value = 0
    for (let bit = 0; bit < BITS; bit++) {
      const x = Math.floor((bit + 0.5) * square)
      if (x >= width) return -1
      const i = (y * width + x) * 4
      if (i + 2 >= pixels.length) return -1
      if ((pixels[i] + pixels[i + 1] + pixels[i + 2]) / 3 > 128) value |= 1 << bit
    }
    return value
  }

  // The band to read, in the decoded picture's pixels.
  function bandSize(videoWidth, referenceWidth) {
    const square = (SQUARE * videoWidth) / referenceWidth
    return { width: Math.ceil(BITS * square), height: Math.ceil(square) }
  }

  root.benchFrameCode = { BITS, SQUARE, decodeRgba, bandSize }
})(globalThis)
