<script lang="ts">
  import ActionTitleHeader from 'components/ActionTitleHeader/ActionTitleHeader.svelte';
  import Button from 'components/Button/Button.svelte';
  import DropdownItem from 'components/Dropdown/DropdownItem.svelte';
  import DropdownLinkList from 'components/Dropdown/DropdownList.svelte';
  import { ROUTES } from 'consts/routes';
  import IconCog from 'icons/cog-12.svg';
  import IconDocument from 'icons/document-12.svg';
  import IconAccount from 'icons/person-12.svg';
  import IconPlus from 'icons/plus-12.svg';
  import ButtonWithDropdown from 'modules/app/components/ButtonWithDropdown/ButtonWithDropdown.svelte';
  import { app } from 'modules/app/store';
  import HostGroup from 'modules/hosts/components/HostGroup/HostGroup.svelte';
  import { fetchAllHosts, hosts } from 'modules/hosts/store/hostsStore';
  import { onMount } from 'svelte';

  onMount(() => {
    app.setBreadcrumbs([
      {
        title: 'Hosts',
        url: ROUTES.HOSTS,
      },
      {
        title: 'All',
        url: '',
      },
    ]);
  });

  fetchAllHosts();

  console.log($hosts);
</script>

<ActionTitleHeader className="container--pull-back">
  <Button asLink style="primary" size="small" href={ROUTES.HOST_ADD}
    >Add host</Button
  >
  <h2 class="t-large" slot="title">All Hosts</h2>
  <ButtonWithDropdown slot="action">
    <svelte:fragment slot="label">Filter <IconPlus /></svelte:fragment>
    <DropdownLinkList slot="content">
      <li>
        <DropdownItem as="button">
          <IconAccount />
          Profile</DropdownItem
        >
      </li>
      <li>
        <DropdownItem as="button">
          <IconDocument />
          Billing</DropdownItem
        >
      </li>
      <li>
        <DropdownItem as="button">
          <IconCog />
          Settings</DropdownItem
        >
      </li>
    </DropdownLinkList>
  </ButtonWithDropdown>
</ActionTitleHeader>

<section class="container--medium-large ">
  {#each $hosts[0].children as host}
    <HostGroup selectedHosts={host} id={host.id}>
      <svelte:fragment slot="title">Host group 1</svelte:fragment>
      <Button asLink href="#" style="outline" size="small" slot="action"
        >View</Button
      >
    </HostGroup>
  {/each}
</section>
