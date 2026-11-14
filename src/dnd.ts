// Fare ile kart sürükleme: yeniden sıralama ve kenar çubuğundaki klasörlere bırakma.

export interface CardDragHandlers {
  canReorder(): boolean;
  onClick(id: string, e: PointerEvent): void;
  onReorder(ids: string[]): void;
  onDropToFolder(folderId: string, draggedId: string): void;
  dragCount(id: string): number;
}

const DRAG_THRESHOLD = 6;
const EDGE = 48;

export function attachCardDrag(grid: HTMLElement, h: CardDragHandlers): void {
  grid.addEventListener("pointerdown", (e: PointerEvent) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    // Kart üzerindeki butonlar, kaydırıcılar ve seçiciler kendi olaylarını çalıştırsın.
    if (target.closest("button, input, select, label, .no-drag")) return;
    const card = target.closest<HTMLElement>(".card");
    if (!card || !grid.contains(card)) return;
    const id = card.dataset.id!;
    e.preventDefault();

    const startX = e.clientX;
    const startY = e.clientY;
    const rect = card.getBoundingClientRect();
    const offX = startX - rect.left;
    const offY = startY - rect.top;
    let dragging = false;
    let ghost: HTMLElement | null = null;
    let folderEl: HTMLElement | null = null;
    let lastOver: HTMLElement | null = null;
    let scrollTimer = 0;
    let lastY = startY;
    const reorder = h.canReorder();

    const setFolder = (el: HTMLElement | null) => {
      if (el === folderEl) return;
      folderEl?.classList.remove("drop-target");
      folderEl = el;
      folderEl?.classList.add("drop-target");
    };

    const autoScroll = () => {
      const r = grid.getBoundingClientRect();
      let dy = 0;
      if (lastY < r.top + EDGE) dy = -Math.ceil((r.top + EDGE - lastY) / 4);
      else if (lastY > r.bottom - EDGE) dy = Math.ceil((lastY - (r.bottom - EDGE)) / 4);
      if (dy) grid.scrollTop += dy;
      scrollTimer = requestAnimationFrame(autoScroll);
    };

    const start = () => {
      dragging = true;
      ghost = card.cloneNode(true) as HTMLElement;
      ghost.classList.remove("selected", "flash");
      ghost.classList.add("drag-ghost");
      const n = h.dragCount(id);
      if (n > 1) {
        const badge = document.createElement("span");
        badge.className = "ghost-count";
        badge.textContent = String(n);
        ghost.append(badge);
      }
      document.body.append(ghost);
      card.classList.add("dragging");
      document.body.style.cursor = "grabbing";
      scrollTimer = requestAnimationFrame(autoScroll);
    };

    const onMove = (ev: PointerEvent) => {
      if (!dragging) {
        if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < DRAG_THRESHOLD) return;
        start();
      }
      lastY = ev.clientY;
      ghost!.style.left = `${ev.clientX - offX}px`;
      ghost!.style.top = `${ev.clientY - offY}px`;

      const under = document.elementFromPoint(ev.clientX, ev.clientY) as HTMLElement | null;
      const nav = under?.closest<HTMLElement>(".nav-item[data-folder]") ?? null;
      setFolder(nav);
      if (nav || !reorder) return;

      const over = under?.closest<HTMLElement>(".card");
      if (!over || over === card || !grid.contains(over) || over === lastOver) {
        if (!over) lastOver = null;
        return;
      }
      lastOver = over;
      const cards = Array.from(grid.children);
      const from = cards.indexOf(card);
      const to = cards.indexOf(over);
      grid.insertBefore(card, from < to ? over.nextSibling : over);
    };

    const onUp = (ev: PointerEvent) => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
      cancelAnimationFrame(scrollTimer);
      document.body.style.cursor = "";
      if (!dragging) {
        if (ev.type === "pointerup") h.onClick(id, ev);
        return;
      }
      ghost?.remove();
      card.classList.remove("dragging");
      const folder = folderEl?.dataset.folder;
      setFolder(null);
      if (folder) {
        h.onDropToFolder(folder, id);
      } else if (reorder) {
        const ids = Array.from(grid.querySelectorAll<HTMLElement>(".card")).map((c) => c.dataset.id!);
        h.onReorder(ids);
      }
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
  });
}
