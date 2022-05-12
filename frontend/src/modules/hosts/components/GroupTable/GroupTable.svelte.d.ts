import { SvelteComponentTyped } from 'svelte';
export interface GroupTableProps {
  [key: string]: any;
  linkToHostDetails: boolean;
}

export default class GroupTable extends SvelteComponentTyped<
  GroupTableProps,
  undefined,
  undefined
> {}
