import { SvelteComponentTyped } from 'svelte';

export interface HierarchyListProps {
  nodes: NavNode[];
  editingId?: string | null;
  handleConfirm?: VoidFunction;
  handleEdit?: (e: KeyboardEvent) => void;
  hideList?: boolean;
}

export interface HierarchyListSlots {
  action: Slot;
  default: Slot;
}

export interface HierarchyListEvents {
  click: (e: MouseEvent) => void;
}

export default class HierarchyList extends SvelteComponentTyped<
  HierarchyListProps,
  HierarchyListEvents,
  HierarchyListSlots
> {}
