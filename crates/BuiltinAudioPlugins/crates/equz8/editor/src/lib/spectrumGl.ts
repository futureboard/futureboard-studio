import type { SpectrumFrame } from '../bridge'
import { MAX_FREQ, MIN_FREQ, freqToProgress } from './eq'

/// WebGL renderer for the analyser overlay.
///
/// Kept out of React on purpose: the frame arrives ~30 Hz and the geometry is
/// rebuilt per frame, so routing it through component state would rerender the
/// whole graph — every band curve, node and label — for a picture that lives
/// entirely inside one canvas. The renderer owns its GL objects, its buffer is
/// allocated once, and `draw` only rewrites vertex data.
///
/// All coordinates are clip space (`-1..1`), with x derived from the *same*
/// log frequency scale the SVG graph uses, so a peak in the spectrum sits under
/// the part of the curve that shapes it.

/// Vertical headroom, as a fraction of the graph, that the spectrum is allowed
/// to occupy. The analyser sits behind the response curve and must not crowd
/// it: a full-scale bin reaches this far up and no further.
const HEIGHT_SCALE = 0.92

const VERTEX_SHADER = `
attribute vec2 position;
varying float vHeight;
void main() {
  // 0 at the floor, 1 at the top — drives the fill gradient without a second
  // attribute or a texture.
  vHeight = (position.y + 1.0) * 0.5;
  gl_Position = vec4(position, 0.0, 1.0);
}
`

const FRAGMENT_SHADER = `
precision mediump float;
varying float vHeight;
uniform vec3 lowColor;
uniform vec3 highColor;
uniform float alpha;
void main() {
  vec3 tint = mix(lowColor, highColor, clamp(vHeight * 1.35, 0.0, 1.0));
  // Fade the base out so the fill reads as energy rising off the floor rather
  // than a solid block sitting on the axis.
  float fade = smoothstep(0.0, 0.22, vHeight);
  gl_FragColor = vec4(tint, alpha * fade);
}
`

function compile(gl: WebGLRenderingContext, type: number, source: string) {
  const shader = gl.createShader(type)
  if (!shader) return null
  gl.shaderSource(shader, source)
  gl.compileShader(shader)
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    gl.deleteShader(shader)
    return null
  }
  return shader
}

export type SpectrumRenderer = {
  draw: (frame: SpectrumFrame | null) => void
  resize: () => void
  dispose: () => void
}

/// Build a renderer over `canvas`, or `null` when WebGL is unavailable — the
/// caller then simply shows no analyser rather than failing the editor.
export function createSpectrumRenderer(
  canvas: HTMLCanvasElement,
): SpectrumRenderer | null {
  const gl = canvas.getContext('webgl', {
    alpha: true,
    antialias: true,
    depth: false,
    premultipliedAlpha: false,
  })
  if (!gl) return null

  const vertex = compile(gl, gl.VERTEX_SHADER, VERTEX_SHADER)
  const fragment = compile(gl, gl.FRAGMENT_SHADER, FRAGMENT_SHADER)
  const program = vertex && fragment ? gl.createProgram() : null
  if (!vertex || !fragment || !program) return null

  gl.attachShader(program, vertex)
  gl.attachShader(program, fragment)
  gl.linkProgram(program)
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    gl.deleteProgram(program)
    return null
  }
  // Linked; the shader objects are no longer referenced by anything else.
  gl.deleteShader(vertex)
  gl.deleteShader(fragment)

  const buffer = gl.createBuffer()
  if (!buffer) {
    gl.deleteProgram(program)
    return null
  }

  const positionLocation = gl.getAttribLocation(program, 'position')
  const lowColorLocation = gl.getUniformLocation(program, 'lowColor')
  const highColorLocation = gl.getUniformLocation(program, 'highColor')
  const alphaLocation = gl.getUniformLocation(program, 'alpha')

  gl.enable(gl.BLEND)
  gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA)

  /// Two vertices per bin (floor + level), drawn as one triangle strip.
  /// Grown on the first frame and reused after — bin count never changes
  /// mid-session, so steady state allocates nothing.
  let vertices = new Float32Array(0)

  const resize = () => {
    const rect = canvas.getBoundingClientRect()
    const dpr = window.devicePixelRatio || 1
    const width = Math.max(1, Math.round(rect.width * dpr))
    const height = Math.max(1, Math.round(rect.height * dpr))
    if (canvas.width === width && canvas.height === height) return
    canvas.width = width
    canvas.height = height
    gl.viewport(0, 0, width, height)
  }

  const draw = (frame: SpectrumFrame | null) => {
    resize()
    gl.clearColor(0, 0, 0, 0)
    gl.clear(gl.COLOR_BUFFER_BIT)
    if (!frame || frame.bins.length === 0) return

    const count = frame.bins.length
    const needed = count * 4
    if (vertices.length !== needed) vertices = new Float32Array(needed)

    const span = Math.log(frame.maxHz / frame.minHz)
    const range = frame.ceilDb - frame.floorDb
    for (let i = 0; i < count; i += 1) {
      // Centre of the band this bin covers, mapped through the graph's own
      // frequency scale so the overlay lines up with the curve above it.
      const hz = frame.minHz * Math.exp((span * (i + 0.5)) / count)
      const x = freqToProgress(Math.min(Math.max(hz, MIN_FREQ), MAX_FREQ)) * 2 - 1
      // Byte back to dB, then dB to a 0..1 height on the published scale.
      const db = frame.floorDb + (frame.bins[i]! / 255) * range
      const level = (db - frame.floorDb) / range
      const top = -1 + level * HEIGHT_SCALE * 2

      const base = i * 4
      vertices[base] = x
      vertices[base + 1] = -1
      vertices[base + 2] = x
      vertices[base + 3] = top
    }

    gl.useProgram(program)
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer)
    gl.bufferData(gl.ARRAY_BUFFER, vertices, gl.DYNAMIC_DRAW)
    gl.enableVertexAttribArray(positionLocation)
    gl.vertexAttribPointer(positionLocation, 2, gl.FLOAT, false, 0, 0)
    // A muted slate blue under the accent-blue curve: the analyser is the
    // backdrop the EQ is drawn against, not a second thing competing with it.
    gl.uniform3f(lowColorLocation, 0.3, 0.4, 0.55)
    gl.uniform3f(highColorLocation, 0.58, 0.7, 0.88)
    gl.uniform1f(alphaLocation, 0.42)
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, count * 2)
  }

  const dispose = () => {
    gl.deleteBuffer(buffer)
    gl.deleteProgram(program)
    // Drop the backing drawing buffer instead of waiting for GC: an editor
    // window can be opened and closed repeatedly in one session, and browsers
    // cap the number of live WebGL contexts.
    gl.getExtension('WEBGL_lose_context')?.loseContext()
  }

  return { draw, resize, dispose }
}
