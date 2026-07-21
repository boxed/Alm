<script>
  let rows = []; let selected = 0; let counter = 1;
  const mk = (start, n) => { const a = []; for (let i = 0; i < n; i++) a.push({ id: start + i, label: 'item ' + (start + i) }); return a; };
  const create = (n) => { const s = counter; counter += n; rows = mk(s, n); };
  const append = (n) => { const s = counter; counter += n; rows = rows.concat(mk(s, n)); };
  const update = () => { rows = rows.map((r, i) => i % 10 === 0 ? { ...r, label: r.label + ' !!!' } : r); };
  const swap = () => { if (rows.length > 1) { const a = rows.slice(); const t = a[1]; a[1] = a[a.length - 1]; a[a.length - 1] = t; rows = a; } };
  const clear = () => { rows = []; selected = 0; };
  const remove = (id) => { rows = rows.filter(r => r.id !== id); };
</script>
<div id="main">
  <button id="create" on:click={() => create(1000)}>create</button>
  <button id="create10k" on:click={() => create(10000)}>create10k</button>
  <button id="append" on:click={() => append(1000)}>append</button>
  <button id="update" on:click={update}>update</button>
  <button id="swap" on:click={swap}>swap</button>
  <button id="clear" on:click={clear}>clear</button>
  <table class="table"><tbody>
    {#each rows as row (row.id)}
      <tr class={row.id === selected ? 'danger' : ''}>
        <td class="col-md-1">{row.id}</td>
        <td class="col-md-4"><a on:click={() => (selected = row.id)}>{row.label}</a></td>
        <td class="col-md-1"><a on:click={() => remove(row.id)}><span class="glyphicon glyphicon-remove"></span></a></td>
        <td class="col-md-6"></td>
      </tr>
    {/each}
  </tbody></table>
</div>
