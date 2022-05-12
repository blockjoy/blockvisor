<script lang="ts">
  import { page } from '$app/stores';
  import ActionTitleHeader from 'components/ActionTitleHeader/ActionTitleHeader.svelte';
  import DropdownItem from 'components/Dropdown/DropdownItem.svelte';
  import DropdownLinkList from 'components/Dropdown/DropdownList.svelte';
  import { ROUTES } from 'consts/routes';
  import IconCog from 'icons/cog-12.svg';
  import IconDocument from 'icons/document-12.svg';
  import IconAccount from 'icons/person-12.svg';
  import IconPlus from 'icons/plus-12.svg';
  import ButtonWithDropdown from 'modules/app/components/ButtonWithDropdown/ButtonWithDropdown.svelte';
  import Pagination from 'modules/app/components/Pagination/Pagination.svelte';
  import NodeGroup from 'modules/nodes/components/NodeGroup/NodeGroup.svelte';
  import {
    fetchNodeById,
    fetchUserById,
    selectedNode,
    selectedUser,
  } from 'modules/nodes/store/nodesStore';

  let currentPage = 1;
  const id = $page.params.id;

  $: hasGroups = false;

  $: {
    fetchNodeById($page.params.id);
    fetchUserById($page.params.id);
  }
</script>

<ActionTitleHeader className="container--pull-back">
  <h2 class="t-large" slot="title">All Nodes</h2>
  <ButtonWithDropdown slot="action">
    <svelte:fragment slot="label">Filter <IconPlus /></svelte:fragment>
    <DropdownLinkList slot="content">
      <li>
        <DropdownItem as="button" href="#">
          <IconAccount />
          Profile</DropdownItem
        >
      </li>
      <li>
        <DropdownItem as="button" href="#">
          <IconDocument />
          Billing</DropdownItem
        >
      </li>
      <li>
        <DropdownItem as="button" href={ROUTES.PROFILE_SETTINGS}>
          <IconCog />
          Settings</DropdownItem
        >
      </li>
    </DropdownLinkList>
  </ButtonWithDropdown>
</ActionTitleHeader>
<NodeGroup {id} nodes={$selectedNode}>
  <svelte:fragment slot="label">Group earnings (USD)</svelte:fragment>
  <svelte:fragment slot="title">{$selectedUser?.name ?? ''}</svelte:fragment>
</NodeGroup>
{#if hasGroups}
  <footer class="nodes-group__footer container--medium-large">
    <Pagination
      pages={6}
      perPage={4}
      itemsTotal={22}
      {currentPage}
      onPageChange={(page) => {
        currentPage = page;
      }}
    />
  </footer>
{/if}

<style>
  .nodes-group {
    &__footer {
      margin-top: 60px;
    }
  }
</style>
