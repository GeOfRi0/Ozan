// Küçük DOM yardımcıları ve modal / bildirim bileşenleri (harici kütüphane yok).

export type Child = Node | string | null | undefined | false;
type Props = Record<string, unknown> & { class?: string; dataset?: Record<string, string> };

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = String(v);
    else if (k === "dataset") Object.assign(el.dataset, v as Record<string, string>);
    else if (k.startsWith("on") && typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v as EventListener);
    } else if (k in el && typeof v !== "string") {
      (el as unknown as Record<string, unknown>)[k] = v;
    } else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c);
  }
  return el;
}

export function $(sel: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(sel);
  if (!el) throw new Error(`Öğe bulunamadı: ${sel}`);
  return el;
}

export const icons = {
  star: `<svg viewBox="0 0 24 24" width="15" height="15"><path fill="currentColor" d="M12 17.3l-6.2 3.7 1.6-7L2 9.2l7.1-.6L12 2l2.9 6.6 7.1.6-5.4 4.8 1.6 7z"/></svg>`,
  close: `<svg viewBox="0 0 24 24" width="13" height="13"><path fill="currentColor" d="M18.3 5.7L12 12l6.3 6.3-1.4 1.4L10.6 13.4 4.3 19.7 2.9 18.3 9.2 12 2.9 5.7 4.3 4.3l6.3 6.3 6.3-6.3z"/></svg>`,
  play: `<svg viewBox="0 0 24 24" width="16" height="16"><path fill="currentColor" d="M8 5v14l11-7z"/></svg>`,
  folder: `<svg viewBox="0 0 24 24" width="16" height="16"><path fill="currentColor" d="M10 4H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-8z"/></svg>`,
  all: `<svg viewBox="0 0 24 24" width="16" height="16"><path fill="currentColor" d="M3 5h18v2H3zm0 6h18v2H3zm0 6h18v2H3z"/></svg>`,
  scissors: `<svg viewBox="0 0 24 24" width="14" height="14"><path fill="currentColor" d="M9.64 7.64a3.5 3.5 0 1 0-1.5 1.5L10.5 11.5l-2.36 2.36a3.5 3.5 0 1 0 1.5 1.5L12 13l6.5 6.5 1.5-1.5-10.36-10.36zM6 8a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3zm0 11a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3zm7.5-7.5L18.5 6.5 20 8l-5 5-1.5-1.5z"/></svg>`,
  key: `<svg viewBox="0 0 24 24" width="13" height="13"><path fill="currentColor" d="M12.65 10A5.99 5.99 0 0 0 7 6c-3.31 0-6 2.69-6 6s2.69 6 6 6a5.99 5.99 0 0 0 5.65-4H17v4h4v-4h2v-4H12.65zM7 14c-1.1 0-2-.9-2-2s.9-2 2-2 2 .9 2 2-.9 2-2 2z"/></svg>`,
  restart: `<svg viewBox="0 0 24 24" width="13" height="13"><path fill="currentColor" d="M12 5V1L7 6l5 5V7c3.31 0 6 2.69 6 6s-2.69 6-6 6-6-2.69-6-6H4c0 4.42 3.58 8 8 8s8-3.58 8-8-3.58-8-8-8z"/></svg>`,
  overlap: `<svg viewBox="0 0 24 24" width="13" height="13"><path fill="currentColor" d="M16 1H4c-1.1 0-2 .9-2 2v14h2V3h12V1zm3 4H8c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h11c1.1 0 2-.9 2-2V7c0-1.1-.9-2-2-2zm0 16H8V7h11v14z"/></svg>`,
};

export function iconSpan(svg: string, cls = "icon"): HTMLSpanElement {
  const s = h("span", { class: cls });
  s.innerHTML = svg;
  return s;
}

// ---------------------------------------------------------------- Bildirimler

export function toast(message: string, kind: "info" | "error" | "ok" = "info", ms = 3500): void {
  const host = document.getElementById("toasts");
  if (!host) return;
  const el = h("div", { class: `toast toast-${kind}` }, message);
  host.append(el);
  setTimeout(() => {
    el.classList.add("out");
    setTimeout(() => el.remove(), 250);
  }, ms);
}

// ---------------------------------------------------------------- Modal

export interface ModalButton {
  id: string;
  label: string;
  kind?: "primary" | "danger" | "ghost";
  onClick?: (close: (id: string) => void) => void | Promise<void>;
}

export interface ModalOptions {
  title: string;
  wide?: boolean;
  body?: Child | Child[];
  buttons: ModalButton[];
  cancelId?: string;
  onOpen?: (root: HTMLElement, close: (id: string) => void) => void;
  onClose?: () => void;
}

let modalDepth = 0;
export const isModalOpen = () => modalDepth > 0;

export function showModal(opts: ModalOptions): Promise<string> {
  return new Promise((resolve) => {
    const host = $("#modal-root");
    const cancelId = opts.cancelId ?? "cancel";
    const bodyChildren = Array.isArray(opts.body) ? opts.body : [opts.body];
    let done = false;

    const close = (id: string) => {
      if (done) return;
      done = true;
      modalDepth = Math.max(0, modalDepth - 1);
      document.removeEventListener("keydown", onKey, true);
      opts.onClose?.();
      backdrop.classList.add("out");
      setTimeout(() => backdrop.remove(), 150);
      resolve(id);
    };

    const buttons = opts.buttons.map((b) =>
      h(
        "button",
        {
          class: `btn btn-${b.kind ?? "ghost"}`,
          onclick: () => {
            if (b.onClick) void b.onClick(close);
            else close(b.id);
          },
          dataset: { id: b.id },
        },
        b.label,
      ),
    );
    const dialog = h(
      "div",
      { class: `modal ${opts.wide ? "modal-wide" : ""}`, role: "dialog", "aria-modal": "true" },
      h("h2", { class: "modal-title" }, opts.title),
      h("div", { class: "modal-body" }, ...bodyChildren),
      h("div", { class: "modal-actions" }, ...buttons),
    );
    const backdrop = h("div", { class: "modal-backdrop" }, dialog);
    backdrop.addEventListener("mousedown", (e) => {
      if (e.target === backdrop) close(cancelId);
    });

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close(cancelId);
      } else if (e.key === "Enter" && e.target instanceof HTMLInputElement && e.target.type === "text") {
        const primary = opts.buttons.find((b) => b.kind === "primary" || b.kind === "danger");
        if (primary) {
          e.preventDefault();
          if (primary.onClick) void primary.onClick(close);
          else close(primary.id);
        }
      }
    };
    document.addEventListener("keydown", onKey, true);
    modalDepth++;
    host.append(backdrop);
    const primaryBtn = buttons.find((_, i) => opts.buttons[i].kind === "primary");
    (primaryBtn ?? buttons[0])?.focus();
    opts.onOpen?.(dialog, close);
  });
}

export async function confirmModal(title: string, message: string, okLabel = "Tamam", danger = false): Promise<boolean> {
  const r = await showModal({
    title,
    body: h("p", {}, message),
    buttons: [
      { id: "cancel", label: "Vazgeç" },
      { id: "ok", label: okLabel, kind: danger ? "danger" : "primary" },
    ],
  });
  return r === "ok";
}

export async function promptModal(title: string, initial = "", placeholder = ""): Promise<string | null> {
  const input = h("input", { class: "input", type: "text", value: initial, placeholder, maxlength: "80" });
  const r = await showModal({
    title,
    body: input,
    buttons: [
      { id: "cancel", label: "Vazgeç" },
      { id: "ok", label: "Kaydet", kind: "primary" },
    ],
    onOpen: () => {
      input.focus();
      input.select();
    },
  });
  const v = input.value.trim();
  return r === "ok" && v ? v : null;
}
