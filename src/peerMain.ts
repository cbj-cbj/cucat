/**
 * Friend (peer) window: renders the remote cat exactly like the original pet window.
 *
 * Input arrives as `kuro-peer-input` events with the same shape as `kuro-input`,
 * look & layout arrive as `kuro-peer-state`, and the engines only run while the
 * window is actually shown (the window is created hidden at startup).
 */
import './style.css';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import {
  DEFAULT_TAIL_KEY_IDS,
  KuroCatEngine,
  type KeyboardOrientation,
  type TailKeyMode,
} from './catEngine';
import { catAudio } from './audio';
import { DEFAULT_BUILTIN_CATS, type CatSkinConfig } from './catSkins';
import type { KeyboardLayoutId } from './keyboardLayout';
import type { CollarColorId, KuroCatEngine3D } from './catEngine3d';

type PeerRenderMode = '2d' | '3d';
type PeerHeadphoneMode = 'auto' | 'on' | 'off';

interface PeerState {
  name?: string;
  catId?: string;
  skin?: CatSkinConfig | null;
  renderMode?: PeerRenderMode;
  keyboardLayout?: string;
  keyboardOrientation?: string;
  scale?: number;
  showKeyBubbles?: boolean;
  tailKeyMode?: string;
  tailKeyIds?: string[];
  collarColor?: string;
  headphoneMode?: PeerHeadphoneMode;
}

type InputEvent =
  | { kind: 'keyDown'; vk: number; scan: number; repeat: boolean }
  | { kind: 'keyUp'; vk: number; scan: number }
  | { kind: 'mouseDown'; button: string }
  | { kind: 'mouseUp'; button: string }
  | { kind: 'mouseWheel'; delta: number }
  | { kind: 'mouseMove'; x: number; y: number; nx: number; ny: number }
  | { kind: 'privacyTyping' };

const canvas = document.getElementById('peer-canvas') as HTMLCanvasElement;
const canvas3d = document.getElementById('peer-canvas-3d') as HTMLCanvasElement;
const nameTag = document.getElementById('peer-name-tag') as HTMLDivElement;

const engine = new KuroCatEngine(canvas);

let engine3d: KuroCatEngine3D | null = null;
let loading3d: Promise<void> | null = null;
let active3d = false;
let webglBlocked = false;
let visible = false;
let privacyTimer: number | null = null;

// Never let the friend's cat purr over our own audio.
catAudio.setEnabled(false);

let catList: CatSkinConfig[] = [...DEFAULT_BUILTIN_CATS];
let peer: PeerState = {
  name: '',
  catId: 'black',
  renderMode: '3d',
  keyboardLayout: '65',
  keyboardOrientation: 'cat',
  scale: 1,
  showKeyBubbles: true,
  tailKeyMode: 'smart',
  tailKeyIds: [...DEFAULT_TAIL_KEY_IDS],
  collarColor: 'none',
  headphoneMode: 'off',
};

function normalizeTailMode(raw: string | undefined): TailKeyMode {
  return raw === 'custom' || raw === 'edge' || raw === 'off' ? raw : 'smart';
}

function normalizeCollar(raw: string | undefined): CollarColorId {
  return raw === 'red' || raw === 'blue' || raw === 'pink' || raw === 'green' ? raw : 'none';
}

function resolveActiveCat(): CatSkinConfig | undefined {
  if (peer.skin && typeof peer.skin.id === 'string' && peer.skin.id) {
    return peer.skin;
  }
  const id = peer.catId || 'black';
  return catList.find((c) => c.id === id) ?? catList[0];
}

function syncPeer3D(
  cat: CatSkinConfig | undefined,
  layout: KeyboardLayoutId,
  orient: KeyboardOrientation,
  tailMode: TailKeyMode,
  tailIds: string[],
  bubbles: boolean,
): void {
  const want3d = (peer.renderMode ?? '3d') !== '2d' && cat?.id !== 'dafeiyu' && !webglBlocked;
  if (!want3d) {
    if (active3d) {
      engine3d?.stop();
      canvas3d.classList.add('hidden');
      canvas.classList.remove('hidden');
      engine.resize();
      if (visible) engine.start();
      active3d = false;
    }
    return;
  }

  if (engine3d) {
    engine3d.showKeyBubbles = bubbles;
    engine3d.setRenderStyle('3d');
    if (cat) engine3d.setCatSkin(cat);
    engine3d.setCollar(normalizeCollar(peer.collarColor));
    // The friend's game detection is local to them, so no headphones are worn here.
    engine3d.setHeadphones(false);
    engine3d.setKeyboardConfig(layout, orient);
    engine3d.setTailKeyConfig(tailMode, tailIds);
    engine3d.setRenderScale(1);
    if (!active3d) {
      engine.stop();
      canvas.classList.add('hidden');
      canvas3d.classList.remove('hidden');
      engine3d.resize();
      if (visible) engine3d.start();
      active3d = true;
    }
    return;
  }

  if (!loading3d) {
    loading3d = import('./catEngine3d')
      .then(({ KuroCatEngine3D }) => {
        engine3d = new KuroCatEngine3D(canvas3d, { embedded: false });
        window.addEventListener('resize', () => engine3d?.resize());
        applyState();
      })
      .catch(() => {
        webglBlocked = true;
      })
      .finally(() => {
        loading3d = null;
      });
  }
}

function applyState(): void {
  const cat = resolveActiveCat();
  const layout: KeyboardLayoutId = peer.keyboardLayout === '108' ? '108' : '65';
  const orient: KeyboardOrientation = peer.keyboardOrientation === 'viewer' ? 'viewer' : 'cat';
  const tailMode = normalizeTailMode(peer.tailKeyMode);
  const tailIds = Array.isArray(peer.tailKeyIds) && peer.tailKeyIds.length > 0
    ? peer.tailKeyIds
    : [...DEFAULT_TAIL_KEY_IDS];
  const bubbles = peer.showKeyBubbles !== false;
  const scale = typeof peer.scale === 'number' ? Math.min(1.5, Math.max(0.2, peer.scale)) : 1;

  engine.showKeyBubbles = bubbles;
  engine.setCatAppearance(cat?.id || 'black', cat);
  engine.setKeyboardConfig(layout, orient);
  engine.setTailKeyConfig(tailMode, tailIds);
  engine.setRenderScale(scale);

  syncPeer3D(cat, layout, orient, tailMode, tailIds, bubbles);

  const label = (peer.name || '').trim();
  if (label) {
    nameTag.textContent = `${label} 🐾`;
    nameTag.classList.remove('hidden');
  } else {
    nameTag.classList.add('hidden');
  }
}

function handleInput(p: InputEvent): void {
  switch (p.kind) {
    case 'keyDown':
      if (active3d && engine3d) engine3d.onKeyDown(p.vk, Boolean(p.repeat));
      else engine.onKeyDown(p.vk, Boolean(p.repeat));
      break;
    case 'keyUp':
      if (active3d && engine3d) engine3d.onKeyUp(p.vk);
      else engine.onKeyUp(p.vk);
      break;
    case 'mouseMove':
      // Guard against a malformed frame sending NaN into the paw springs.
      if (!Number.isFinite(p.nx) || !Number.isFinite(p.ny)) break;
      if (active3d && engine3d) engine3d.onGlobalMouseMove(p.nx, p.ny);
      else engine.onGlobalMouseMove(p.nx, p.ny);
      break;
    case 'mouseDown':
      if (active3d && engine3d) engine3d.onMouseDown(p.button);
      else engine.onMouseDown(p.button);
      break;
    case 'mouseUp':
      if (active3d && engine3d) engine3d.onMouseUp(p.button);
      else engine.onMouseUp(p.button);
      break;
    case 'mouseWheel':
      if (active3d && engine3d) engine3d.onMouseWheel();
      else engine.onMouseWheel();
      break;
    case 'privacyTyping':
      // Password entry on the friend's side: cover the keyboard for a moment.
      if (active3d && engine3d) engine3d.onPrivacyTyping();
      else engine.onPrivacyTyping();
      holdPrivacyMask();
      break;
  }
}

function holdPrivacyMask(): void {
  engine.setPrivacyMode(true);
  engine3d?.setPrivacyMode(true);
  if (privacyTimer !== null) window.clearTimeout(privacyTimer);
  privacyTimer = window.setTimeout(() => {
    privacyTimer = null;
    engine.setPrivacyMode(false);
    engine3d?.setPrivacyMode(false);
  }, 4000);
}

function startEngines(): void {
  if (active3d && engine3d) engine3d.start();
  else engine.start();
}

function stopEngines(): void {
  engine.stop();
  engine3d?.stop();
}

async function bootstrap(): Promise<void> {
  applyState();

  // Dragging: this window is click-through until Alt is held (Rust toggles that),
  // so a pointer event here means the user is intentionally repositioning it.
  let saveTimer: number | null = null;
  const persistPosition = (): void => {
    if (saveTimer !== null) window.clearInterval(saveTimer);
    const started = Date.now();
    saveTimer = window.setInterval(() => {
      void invoke('save_peer_window_pos').catch(() => undefined);
      if (Date.now() - started > 12000 && saveTimer !== null) {
        window.clearInterval(saveTimer);
        saveTimer = null;
      }
    }, 700);
  };
  document.addEventListener('pointerdown', (event) => {
    if (event.button !== 0) return;
    void import('@tauri-apps/api/window')
      .then(({ getCurrentWindow }) => getCurrentWindow().startDragging())
      .catch(() => undefined);
    persistPosition();
  });
  document.addEventListener('pointerup', () => {
    void invoke('save_peer_window_pos').catch(() => undefined);
    if (saveTimer !== null) {
      window.clearInterval(saveTimer);
      saveTimer = null;
    }
  });

  await listen<boolean>('kuro-peer-visibility', (ev) => {
    visible = Boolean(ev.payload);
    if (visible) {
      applyState();
      startEngines();
    } else {
      stopEngines();
    }
  });

  await listen<PeerState>('kuro-peer-state', (ev) => {
    const next = ev.payload;
    if (!next || typeof next !== 'object') return;
    peer = { ...peer, ...next };
    applyState();
  });

  await listen<InputEvent>('kuro-peer-input', (ev) => {
    const payload = ev.payload;
    if (!payload || typeof payload !== 'object') return;
    handleInput(payload);
  });

  // The window may already be visible when this page finishes loading.
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    visible = await getCurrentWindow().isVisible();
    if (visible) startEngines();
  } catch {
    visible = true;
    startEngines();
  }
}

void bootstrap();
