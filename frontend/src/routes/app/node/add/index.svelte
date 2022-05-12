<script lang="ts">
  import StepLabel from 'components/StepLabel/StepLabel.svelte';
  import { fade } from 'svelte/transition';
  import StepList from 'components/StepList/StepList.svelte';
  import { MetaTags } from 'svelte-meta-tags';

  import { app } from 'modules/app/store';
  import { onDestroy, onMount } from 'svelte';
  import NodeAdd from 'modules/nodes/components/NodeAdd/NodeAdd.svelte';
  import { ROUTES } from 'consts/routes';

  let currentStep = 1;

  const setStep = (step: number) => {
    currentStep = step;
  };

  onMount(() => {
    app.setBreadcrumbs([
      {
        title: 'Nodes',
        url: ROUTES.NODES,
      },
      {
        title: 'Add a node',
        url: '',
      },
    ]);
  });
</script>

<MetaTags title="Add a new node | BlockVisor" />

<div class="add-node container--large grid grid-spacing">
  <aside class="add-node__sidebar">
    <StepList {currentStep}>
      <li in:fade={{ duration: 250 }}>
        <StepLabel {setStep} {currentStep} step={1}>Select a network</StepLabel>
      </li>
      <li in:fade={{ duration: 250, delay: 220 }}>
        <StepLabel {setStep} {currentStep} step={2}>Select node type</StepLabel>
      </li>
      <li in:fade={{ duration: 250, delay: 440 }}>
        <StepLabel {setStep} {currentStep} step={3}>Add a host</StepLabel>
      </li>
      <li in:fade={{ duration: 250, delay: 660 }}>
        <StepLabel {setStep} {currentStep} step={4}>Provision host</StepLabel>
      </li>
      <li in:fade={{ duration: 250, delay: 660 }}>
        <StepLabel {setStep} {currentStep} step={5}>Review & finish</StepLabel>
      </li>
    </StepList>
  </aside>

  <NodeAdd {setStep} {currentStep} />
</div>

<style>
  .add-node {
    align-items: flex-start;

    @media (--screen-large) {
      height: 100%;
    }

    &__sidebar {
      grid-column: span 12;
      padding-top: 40px;
      text-align: center;
      overflow-x: hidden;

      @media (--screen-large) {
        text-align: left;
        grid-column: span 3;
        height: 100%;
        border-right: 1px solid theme(--color-text-5-o10);
      }
    }
  }
</style>
