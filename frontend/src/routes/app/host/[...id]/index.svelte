<script>
  import { page } from '$app/stores';

  import BackButton from 'modules/app/components/BackButton/BackButton.svelte';
  import DetailsHeader from 'modules/hosts/components/DetailsHeader/DetailsHeader.svelte';
  import DetailsLayout from 'modules/hosts/components/DetailsLayout/DetailsLayout.svelte';
  import DetailsTable from 'modules/hosts/components/DetailsTable/DetailsTable.svelte';
  import { useForm } from 'svelte-use-form';

  const form = useForm();

  const id = $page.params.id;
</script>

<DetailsLayout>
  <BackButton slot="nav" />

  <!-- <DetailsHeader {form} {id} state="issue" /> -->

  <!-- <DetailsTable /> -->
</DetailsLayout>
