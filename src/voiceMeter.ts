/**
 * Microphone loudness meter for the talking cat.
 *
 * Runs in the webview (Web Audio API); games/OBS can keep the same microphone
 * open at the same time — Windows allows multiple captures of one device.
 */
export class VoiceMeter {
  private ctx: AudioContext | null = null;
  private stream: MediaStream | null = null;
  private analyser: AnalyserNode | null = null;
  private data: Float32Array<ArrayBuffer> | null = null;
  private raf = 0;
  private running = false;

  /** Smoothed 0..1 loudness: 0 silent, ~0.3 normal speech, ~0.8 shouting. */
  public onLevel: ((level: number) => void) | null = null;
  public onError: ((message: string) => void) | null = null;

  public get isRunning(): boolean {
    return this.running;
  }

  public async start(): Promise<boolean> {
    if (this.running) return true;
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: false,
        },
      });
      const Ctor: typeof AudioContext =
        window.AudioContext ??
        (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      const ctx = new Ctor();
      const source = ctx.createMediaStreamSource(stream);
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 1024;
      analyser.smoothingTimeConstant = 0.45;
      source.connect(analyser);
      if (ctx.state === 'suspended') {
        void ctx.resume().catch(() => undefined);
      }

      this.stream = stream;
      this.ctx = ctx;
      this.analyser = analyser;
      this.data = new Float32Array(new ArrayBuffer(analyser.fftSize * 4));
      this.running = true;
      this.loop();
      return true;
    } catch (err) {
      this.running = false;
      this.onError?.(err instanceof Error ? err.message : String(err));
      return false;
    }
  }

  private loop = (): void => {
    if (!this.running || !this.analyser || !this.data) return;
    this.analyser.getFloatTimeDomainData(this.data);
    let sum = 0;
    for (let i = 0; i < this.data.length; i += 1) {
      const v = this.data[i];
      sum += v * v;
    }
    const rms = Math.sqrt(sum / this.data.length);
    // Speech RMS sits around 0.02–0.15; map onto 0..1 with a soft knee.
    const level = Math.min(1, Math.max(0, (rms - 0.006) / 0.12));
    this.onLevel?.(level);
    this.raf = window.requestAnimationFrame(this.loop);
  };

  public stop(): void {
    this.running = false;
    if (this.raf) window.cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.stream?.getTracks().forEach((track) => track.stop());
    this.stream = null;
    if (this.ctx) {
      void this.ctx.close().catch(() => undefined);
    }
    this.ctx = null;
    this.analyser = null;
    this.data = null;
    this.onLevel?.(0);
  }
}

export const voiceMeter = new VoiceMeter();
