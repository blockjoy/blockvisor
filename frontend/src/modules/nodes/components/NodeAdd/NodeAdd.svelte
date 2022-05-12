<script lang="ts">
  import Layout from './Layout.svelte';

  import { onMount } from 'svelte';

  import { useForm } from 'svelte-use-form';

  import StepFirst from './StepFirst.svelte';
  import StepSecond from './StepSecond.svelte';
  import StepThird from './StepThird.svelte';
  import StepFourth from './StepFourth.svelte';
  import StepFifth from './StepFifth.svelte';
  import GhostForm from './GhostForm.svelte';

  const components = [StepFirst, StepSecond, StepThird, StepFourth, StepFifth];
  const layoutProps = [
    { title: 'Add a Node', size: ' large' },
    { title: 'Add a Node', size: 'large' },
    { title: 'Add a Node', size: 'large' },
    { title: 'Provision Host', size: 'large' },
    { title: 'Review & Finish', size: 'large' },
  ];

  let activeComponent = null;

  let swarmKey = { accepted: [], rejected: [] };

  const form = useForm({
    network: { initial: '' },
    nodeType: { initial: '' },
    host: { initial: '' },
  });

  const handleFilesSelect = (e) => {
    e.preventDefault();
    e.stopPropagation();

    const { acceptedFiles, fileRejections } = e.detail;
    swarmKey.accepted = acceptedFiles;
    swarmKey.rejected = fileRejections;
  };

  const handleFilesRemove = (e) => {
    e.preventDefault();
    e.stopPropagation();

    swarmKey = {
      accepted: [],
      rejected: [],
    };
  };

  export let currentStep = 1;
  export let setStep;

  const updateViewportComponent = () => {
    activeComponent = components[currentStep - 1];
  };

  onMount(updateViewportComponent);
</script>

{#if activeComponent === components[currentStep - 1]}
  <Layout
    on:outroend={updateViewportComponent}
    {...layoutProps[currentStep - 1]}
  >
    <svelte:component
      this={activeComponent}
      {handleFilesSelect}
      {handleFilesRemove}
      {swarmKey}
      {setStep}
      {form}
    >
      <GhostForm />
    </svelte:component>
  </Layout>
{/if}
