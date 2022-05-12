<script lang="ts">
  import ActionTitleHeader from 'components/ActionTitleHeader/ActionTitleHeader.svelte';
  import ButtonWithDropdown from 'modules/app/components/ButtonWithDropdown/ButtonWithDropdown.svelte';
  import { app } from 'modules/app/store';
  import DropdownLinkList from 'components/Dropdown/DropdownList.svelte';
  import DropdownItem from 'components/Dropdown/DropdownItem.svelte';

  import IconAccount from 'icons/person-12.svg';
  import IconDocument from 'icons/document-12.svg';
  import IconCog from 'icons/cog-12.svg';
  import IconPlus from 'icons/plus-12.svg';
  import { useForm } from 'svelte-use-form';
  import { onMount } from 'svelte';
  import { ROUTES } from 'consts/routes';
  import HostGroup from 'modules/hosts/components/HostGroup/HostGroup.svelte';
  import Button from 'components/Button/Button.svelte';
  import { selectedHosts, fetchHostById } from 'modules/hosts/store/hostsStore';
  import { page } from '$app/stores';
import DetailsTable from 'modules/hosts/components/DetailsTable/DetailsTable.svelte';
import DetailsHeader from 'modules/hosts/components/DetailsHeader/DetailsHeader.svelte';

  const id = $page.params.id;
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

  $: {
    fetchHostById($page.params.id);
  }

  const form = useForm();
</script>
<ActionTitleHeader className="container--pull-back">
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

<DetailsHeader data={$selectedHosts} {form} state="consensus" {id} />
<DetailsTable data={$selectedHosts} />

<section class="container--medium-large ">
  <HostGroup selectedHosts={$selectedHosts} id={$page.params.id}>
    <svelte:fragment slot="title">Host group 1</svelte:fragment>
    <Button asLink href="#" style="outline" size="small" slot="action"
      >View</Button
    >
  </HostGroup>
</section>
