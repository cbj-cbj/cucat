/**
 * Floating Chinese characters that pop out of the cat's mouth while it "talks".
 *
 * DOM based: the pet window is a transparent webview, so plain absolutely
 * positioned spans are cheaper (and crisper) than text drawn into the canvas.
 */
const CHAR_POOL =
  '你好呀今天真棒哈哈嗯呢诶哦哇真的吗好啊对对不错哈嘿嘛啦吧呀咦喔嗷嘿嘿耶赞' +
  '说讲聊喵呜呼噜撸猫可爱漂亮厉害加油冲鸭捏贴贴抱抱想你想吃好饿困困睡睡';

export class SpeechBubbles {
  private lastSpawn = 0;

  constructor(private readonly layer: HTMLElement | null) {}

  /** Emit at most one character per `minGapMs`, ignoring bursts. */
  public spawnAt(
    x: number,
    y: number,
    strength = 1,
    minGapMs = 110,
  ): void {
    if (!this.layer) return;
    const now = performance.now();
    if (now - this.lastSpawn < minGapMs) return;
    this.lastSpawn = now;

    const el = document.createElement('span');
    el.className = 'speech-char';
    el.textContent = CHAR_POOL[Math.floor(Math.random() * CHAR_POOL.length)];
    const size = 14 + Math.random() * 8 + Math.min(1, strength) * 5;
    el.style.left = `${x + (Math.random() * 26 - 13)}px`;
    el.style.top = `${y + (Math.random() * 12 - 6)}px`;
    el.style.fontSize = `${size.toFixed(1)}px`;
    el.style.setProperty('--drift', `${(Math.random() * 64 - 32).toFixed(0)}px`);
    el.style.setProperty('--tilt', `${(Math.random() * 26 - 13).toFixed(0)}deg`);
    this.layer.appendChild(el);
    window.setTimeout(() => el.remove(), 1800);
  }

  public clear(): void {
    this.layer?.replaceChildren();
  }
}
