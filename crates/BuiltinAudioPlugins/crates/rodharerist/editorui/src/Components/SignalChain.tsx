import { useEffect, useRef } from "react";
import {
  DndContext,
  PointerSensor,
  closestCenter,
  useDraggable,
  useDroppable,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  horizontalListSortingStrategy,
  useSortable,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import {
  categories,
  icons,
  models,
  rackFromPath,
  type CategoryId,
} from "../data";
import { BlockMenu, type MenuItem } from "./BlockMenu";

type SignalChainProps = {
  pathOrder: CategoryId[];
  activeCat: CategoryId;
  stageModels: Record<CategoryId, string>;
  bypassed: Partial<Record<CategoryId, boolean>>;
  /** Category whose settings are on the clipboard, if any. */
  clipboardCat: CategoryId | null;
  onSelectCategory: (cat: CategoryId) => void;
  onToggleModule: (cat: CategoryId) => void;
  onReorderPath: (next: CategoryId[]) => void;
  onCopySettings: (cat: CategoryId) => void;
  onPasteSettings: (cat: CategoryId) => void;
  onResetModule: (cat: CategoryId) => void;
};

const EMPTY_PATH_DROP_ID = "path-empty";
const RACK_DROP_ID = "rack-drop";

function modelLabel(cat: CategoryId, modelId: string): string {
  const list = models[cat] ?? [];
  const found = list.find((m) => m.id === modelId) ?? list[0];
  return found?.short ?? found?.name ?? "—";
}

function PathModule({
  cat,
  selected,
  isBypassed,
  label,
  title,
  menuItems,
  onSelect,
  onToggle,
}: {
  cat: CategoryId;
  selected: boolean;
  isBypassed: boolean;
  label: string;
  title: string;
  menuItems: MenuItem[];
  onSelect: () => void;
  onToggle: () => void;
}) {
  const c = categories[cat];
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } =
    useSortable({ id: cat });

  return (
    <div className="module-wrap">
      <div
        ref={setNodeRef}
        className={`module${selected ? " selected" : ""}${isBypassed ? " bypassed" : ""}`}
        style={{
          ["--mc" as string]: c.color,
          transform: CSS.Transform.toString(transform),
          transition,
          opacity: isDragging ? 0.4 : 1,
        }}
        {...attributes}
        {...listeners}
        onClick={onSelect}
        title={title}
      >
        <button
          className="blk-power"
          title={isBypassed ? "Enable block" : "Bypass block"}
          aria-label={isBypassed ? `Enable ${c.name}` : `Bypass ${c.name}`}
          aria-pressed={!isBypassed}
          type="button"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={(e) => {
            e.stopPropagation();
            onToggle();
          }}
        />
        <BlockMenu label={`${c.name} block options`} items={menuItems} />
        <div className="ic">
          <svg
            width="22"
            height="22"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            dangerouslySetInnerHTML={{ __html: icons[c.node] ?? "" }}
          />
        </div>
        <div className="mtext">
          <span className="mtitle">{c.short}</span>
          <span className="mmodel">{label}</span>
        </div>
        {/* State is never colour-only: a bypassed block says so in words. */}
        {isBypassed && <span className="blk-state">BYP</span>}
      </div>
    </div>
  );
}

function RackItem({
  cat,
  onAdd,
}: {
  cat: CategoryId;
  onAdd: (cat: CategoryId) => void;
}) {
  const c = categories[cat];
  const { attributes, listeners, setNodeRef, transform, isDragging } = useDraggable({
    id: `rack-${cat}`,
    data: { fromRack: true, cat },
  });

  return (
    <button
      ref={setNodeRef}
      type="button"
      className="rack-item"
      style={{
        ["--mc" as string]: c.color,
        transform: CSS.Translate.toString(transform),
        opacity: isDragging ? 0.4 : 1,
      }}
      {...attributes}
      {...listeners}
      onClick={() => onAdd(cat)}
      title={`Add ${c.name} to path`}
    >
      {c.short}
    </button>
  );
}

function EmptyPathDropTarget() {
  const { setNodeRef, isOver } = useDroppable({ id: EMPTY_PATH_DROP_ID });
  return (
    <div ref={setNodeRef} className={`path-empty${isOver ? " drag-over" : ""}`}>
      Empty path — add blocks from the rack
    </div>
  );
}

/**
 * The rack, and the drop target that takes a block back out of the path.
 *
 * Rendered even when every stage is in the path (nothing left to add), because
 * it is still the place a block is dragged to in order to leave.
 */
const ADD_GROUPS: { label: string; cats: CategoryId[] }[] = [
  { label: "Dynamics", cats: ["dyn", "comp", "comp2"] },
  { label: "Drive", cats: ["wah", "dist", "dist2", "amp"] },
  { label: "EQ", cats: ["eq", "eq2"] },
  { label: "Mod", cats: ["mod", "mod2"] },
  { label: "Time", cats: ["delay", "delay2", "verb"] },
  { label: "Cab", cats: ["cab"] },
];

function Rack({
  rack,
  onAdd,
}: {
  rack: CategoryId[];
  onAdd: (cat: CategoryId) => void;
}) {
  const { setNodeRef, isOver } = useDroppable({ id: RACK_DROP_ID });
  const available = new Set(rack);
  return (
    <div ref={setNodeRef} className={`rack${isOver ? " drag-over" : ""}`}>
      <span className="rack-label">Add Block</span>
      {ADD_GROUPS.map((group) => {
        const cats = group.cats.filter((cat) => available.has(cat));
        if (cats.length === 0) return null;
        return (
          <div key={group.label} className="rack-group">
            <span className="rack-group-label">{group.label}</span>
            {cats.map((cat) => (
              <RackItem key={cat} cat={cat} onAdd={onAdd} />
            ))}
          </div>
        );
      })}
      {rack.length === 0 && (
        <span className="rack-empty">
          Every block is in the path — drag one here to remove it
        </span>
      )}
    </div>
  );
}

export function SignalChain({
  pathOrder,
  activeCat,
  stageModels,
  bypassed,
  clipboardCat,
  onSelectCategory,
  onToggleModule,
  onReorderPath,
  onCopySettings,
  onPasteSettings,
  onResetModule,
}: SignalChainProps) {
  const svgRef = useRef<SVGSVGElement>(null);
  const rowRef = useRef<HTMLDivElement>(null);
  const rack = rackFromPath(pathOrder);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
  );

  useEffect(() => {
    const draw = () => {
      const svg = svgRef.current;
      const row = rowRef.current;
      if (!svg || !row) return;
      while (svg.firstChild) svg.removeChild(svg.firstChild);

      const nodes = row.querySelectorAll<HTMLElement>(".module");
      if (nodes.length < 2) return;
      const box = svg.getBoundingClientRect();

      for (let i = 0; i < nodes.length - 1; i++) {
        const a = nodes[i]!.getBoundingClientRect();
        const b = nodes[i + 1]!.getBoundingClientRect();
        const x1 = a.right - box.left - 1;
        const y1 = a.top + a.height / 2 - box.top;
        const x2 = b.left - box.left + 1;
        const y2 = b.top + b.height / 2 - box.top;
        const path = document.createElementNS(
          "http://www.w3.org/2000/svg",
          "path",
        );
        // A patch cable, not a schematic wire: sag the run between the two
        // jacks with a shallow quadratic droop, scaled to the gap so short
        // runs stay almost straight.
        const sag = Math.min(10, Math.max(3, (x2 - x1) * 0.35));
        const midX = (x1 + x2) / 2;
        const midY = Math.max(y1, y2) + sag;
        path.setAttribute("d", `M ${x1} ${y1} Q ${midX} ${midY} ${x2} ${y2}`);
        path.setAttribute("fill", "none");
        const dim =
          nodes[i]!.classList.contains("bypassed") ||
          nodes[i + 1]!.classList.contains("bypassed");
        path.setAttribute(
          "stroke",
          dim ? "rgba(255,255,255,0.07)" : "rgba(255,255,255,0.22)",
        );
        path.setAttribute("stroke-width", "2");
        path.setAttribute("stroke-linecap", "round");
        svg.appendChild(path);
      }
    };

    draw();
    const t = window.setTimeout(draw, 60);
    window.addEventListener("resize", draw);
    return () => {
      window.clearTimeout(t);
      window.removeEventListener("resize", draw);
    };
  }, [activeCat, bypassed, pathOrder, stageModels]);

  const removeFromPath = (cat: CategoryId) => {
    onReorderPath(pathOrder.filter((c) => c !== cat));
  };

  const addToPath = (cat: CategoryId, at?: number) => {
    if (pathOrder.includes(cat)) return;
    const next = [...pathOrder];
    if (at === undefined || at < 0 || at > next.length) next.push(cat);
    else next.splice(at, 0, cat);
    onReorderPath(next);
  };

  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over) return;
    const dragData = active.data.current as
      | { fromRack?: boolean; cat?: CategoryId }
      | undefined;

    if (dragData?.fromRack) {
      const cat = dragData.cat as CategoryId;
      // Dropped back where it started: the rack is the "not in the path" area,
      // so this is a cancelled drag, not an append.
      if (over.id === RACK_DROP_ID) return;
      if (over.id === EMPTY_PATH_DROP_ID) {
        addToPath(cat);
        return;
      }
      const overIndex = pathOrder.indexOf(over.id as CategoryId);
      addToPath(cat, overIndex >= 0 ? overIndex : undefined);
      return;
    }

    const activeCatId = active.id as CategoryId;
    // Dragging a block onto the rack is how it leaves the path — the mirror of
    // dragging one out of the rack to add it.
    if (over.id === RACK_DROP_ID) {
      removeFromPath(activeCatId);
      return;
    }
    const overCatId = over.id as CategoryId;
    if (activeCatId === overCatId) return;
    const oldIndex = pathOrder.indexOf(activeCatId);
    const newIndex = pathOrder.indexOf(overCatId);
    if (oldIndex < 0 || newIndex < 0) return;
    onReorderPath(arrayMove(pathOrder, oldIndex, newIndex));
  };

  return (
    <section className="chain">
      <span className="chain-title">Path</span>
      <span className="chain-hint">
        Drag to reorder · drag to the rack to remove · ⋮ for block options
      </span>
      <DndContext
        sensors={sensors}
        collisionDetection={closestCenter}
        onDragEnd={handleDragEnd}
      >
        {/* Scroller: with nine stages the path can outgrow the window, so the
            board pans horizontally. The connector SVG lives INSIDE the
            scrolled track (sized to the content, not the viewport) so the
            wires move with the blocks. */}
        <div className="chain-scroll">
          <div className="chain-track">
            <svg className="chain-svg" ref={svgRef} />
            <div className="chain-row" ref={rowRef} id="chain-row">
              {pathOrder.length === 0 && <EmptyPathDropTarget />}

              <SortableContext items={pathOrder} strategy={horizontalListSortingStrategy}>
                {pathOrder.map((cat) => {
              const c = categories[cat];
              const selected = cat === activeCat;
              const isBypassed = !!bypassed[cat];
              const mid = stageModels[cat] ?? models[cat][0]?.id ?? "";
              const label = modelLabel(cat, mid);
              return (
                <PathModule
                  key={cat}
                  cat={cat}
                  selected={selected}
                  isBypassed={isBypassed}
                  label={label}
                  title={`${c.name}: ${models[cat].find((m) => m.id === mid)?.name ?? label}`}
                  menuItems={[
                    {
                      label: isBypassed ? "Enable Block" : "Bypass Block",
                      onSelect: () => onToggleModule(cat),
                    },
                    {
                      label: "Copy Settings",
                      onSelect: () => onCopySettings(cat),
                      separatorBefore: true,
                    },
                    {
                      label: "Paste Settings",
                      // Settings only transfer between blocks of the same
                      // category — the models have different parameter sets.
                      disabled: clipboardCat !== cat,
                      onSelect: () => onPasteSettings(cat),
                    },
                    {
                      label: "Reset Effect",
                      onSelect: () => onResetModule(cat),
                    },
                    {
                      label: "Remove from Path",
                      onSelect: () => removeFromPath(cat),
                      separatorBefore: true,
                      destructive: true,
                    },
                  ]}
                  onSelect={() => onSelectCategory(cat)}
                  onToggle={() => onToggleModule(cat)}
                />
              );
            })}
              </SortableContext>
            </div>
          </div>
        </div>

        <Rack rack={rack} onAdd={addToPath} />
      </DndContext>
    </section>
  );
}
