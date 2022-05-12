import { SvelteComponentTyped } from 'svelte';
export interface HosetDataRowProps {
  name: string;
  status: HostState;
  ipAddr: string;
  id: string;
  createdAt: string;
  linkToHostDetails?: boolean;
}

export default class HosetDataRow extends SvelteComponentTyped<
  HosetDataRowProps,
  undefined,
  undefined
> {}
