# X2 — Atribuição de Wait-For-Idle e paginação

O GPU Trace da cena pesada na RTX 4070 mostrou cerca de 85 ms de GPU por quadro, 43–45% de FE: WFI, SMs a 11% e PCIe a 19,6%. O experimento mede quanto desse tempo coincide com as ordens do título e quanto coincide com as cópias, passes e barreiras que o Victory usa para manter as duplicatas coerentes.

## Captura em release

1. No `.bat` que inicia o Victory pelo Nsight Graphics GPU Trace, acrescente `VICTORY_RF_MARKERS=1` às atribuições da opção `--env` que vai para o processo capturado. Se for a única variável, o argumento fica `--env "VICTORY_RF_MARKERS=1"`. Preserve as demais variáveis da captura.
2. Capture a mesma cena pesada. Abra **Trace Analysis** e agrupe pela coluna **Range**. Compare os intervalos `vx:` e `title:` dentro de cada fila. Use os intervalos `list:` para ver a origem da lista. Abra os filhos dos intervalos antes de somar tempos, porque os pais incluem os filhos. `vx:thin` é um intervalo de contexto para a lista traduzida e contém os comandos marcados do título.
3. Examine **FE: WFI** por intervalo, em particular `fe__cycles_stalled_cmd_wfi_queue_sync`. Compare os ciclos e a fração de WFI nos passes de coerência, nas barreiras específicas e nos trechos do título. Examine também os comandos adjacentes quando o stall aparecer no limite de dois intervalos ou em uma lacuna entre listas.
4. Alinhe os trechos com o log `rf-x2:`. Compare `uav_per_frame`, `transition_per_frame`, `aliasing_per_frame` e as listas por origem com o WFI. Picos junto de `vx:coh.*`, `vx:card.steps`, `vx:prefetch` ou suas barreiras dão uma hipótese sobre o custo de sincronizar as duplicatas. Se o custo se concentrar nos trechos do título, veja também os campos `title_*_per_frame`, que contam as barreiras emitidas ao traduzir ordens de sincronização do título.
5. Repita com `VICTORY_RF_MARKERS=2` para detalhar as trocas de PSO. Para medir a pressão de memória sem a instrumentação de ranges, use `VICTORY_RF_X2_MEMSTATS=1` com `VICTORY_RF_MARKERS` ausente.

Os dois modos emitem `BeginEvent` e `EndEvent` com metadado 0 e texto UTF-16. No modo 1 cada bloco reúne até 32 comandos de trabalho, contando uma execução indireta agrupada como um comando host. A etiqueta `title:<hash>` usa os primeiros 16 dígitos hexadecimais da chave da biblioteca de PSOs do primeiro comando do bloco. No modo 2, começa um novo intervalo quando muda o PSO dos draws ou dispatches gravados. `title:unbound` cobre o início da lista até o primeiro trabalho com PSO. Cópias, clears e sincronizações do título permanecem no trecho corrente.

## Inventário de ranges

Os caminhos da tabela são relativos a `crates/victory-gpu/src/`.

| Nome do range | Arquivo e função | Trabalho marcado |
| --- | --- | --- |
| `list:title` | `host.rs:Host::make_list`, `HostList::reset`, `HostList::close`, `HostList::still_left_open`; `list.rs:boxed`, `fresh_list` | Lista host do título, inclusive as partes cortadas e as provenientes do thin |
| `list:aux.coherence` | `host.rs:Host::new_list`, `Host::card_before`; `host/card_steps.rs:Host::own_list` | Listas auxiliares, incluindo upload, hash, compare e etapas de duplicatas |
| `list:aux.copy` | `host.rs:Host::demand_fetch`, `Host::card_back`, `Host::relay_card_back`, `Host::presenter`, `Host::present_planes`; `texture/ahead.rs:Worker::copy`; `draw.rs:State::early_textures` (criação de `BakeList`) | Readback, relay, cópia da apresentação e uploads auxiliares de texturas |
| `list:prelude` | `host.rs:HostList::prelude` | Estados e uploads imediatamente antes de uma lista do título |
| `title:unbound`, `title:<hash>` | `host.rs:HostList::gpu_pass`, `HostList::record_held`; `draw.rs:State::bind`; `pipeline/host.rs:HostPipeline::new` | Draws, dispatches e execuções indiretas. A chave vem do PSO que foi vinculado. |
| `vx:thin` | `list/thin.rs:back_list`; `list.rs:boxed`, `fresh_list`; `rf_x2.rs:Recording::start_title`, `reuse_title` | Vida da lista host produzida pela tradução thin |
| `vx:coh.send`, `vx:coh.hash`, `vx:coh.compare` | `host.rs:Host::card_before` | Bytes copiados para a GPU, hash das peças enviadas e compare depois das listas do título |
| `vx:coh.send.bulk`, `vx:coh.send.scatter` | `host/send.rs:Own::record` | Dispatches de envio dos tiles e dos pares de palavras |
| `vx:coh.upcheck` | `host/upcheck.rs:record` | Verificação opcional de bytes antes de enviá-los |
| `vx:coh.fetch` | `host.rs:Host::card_back`, `Host::demand_fetch` | Cópias de readback das peças alteradas e leituras por demanda |
| `vx:relay` | `host.rs:Host::relay_card_back` | Readback do relay na fila direta ou na de cópia |
| `vx:coh.place` | `host/place.rs:HostList::place_copy`, `place_source`, `placed_constants` | Cópias que preservam o valor no ponto em que a lista o lê |
| `vx:card.steps` | `host/card_steps.rs:record` | Cópias intermediárias entre duplicatas do título |
| `vx:prefetch` | `host/prefetch.rs:record` | Cópia antecipada de peças sinalizadas pelo compare |
| `vx:indirect.clamp`, `vx:indirect.clamp.batch` | `indirect.rs:record_clamp_as`; `indirect/batch.rs:record` | Limitação dos argumentos indiretos, individual ou em lote |
| `vx:ambient.keep`, `vx:ambient.mean`, `vx:ambient.air`, `vx:ambient.place`, `vx:ambient.gather`, `vx:ambient.raise`, `vx:ambient.reflect`, `vx:ambient.other` | `ambient.rs:after`; `rf_x2.rs:ambient` | Dispatch por etapa e a barreira UAV que o segue. `other` é a etiqueta de reserva. |
| `vx:tex.copy` | `texture.rs:alias`, `PreludeUpload::record`, `record_upload`; `texture/ahead.rs:Job::record`; `host.rs:HostList::textures_back` | Upload, cópia entre aliases e readback de textura |
| `vx:tex.depth`, `vx:tex.zero` | `host.rs:record_twin_copy`, `HostList::pass_plane`; `texture.rs:zeros`, `card_writes` | Transferência entre depth twins, recomposição do plano de profundidade e inicialização de texturas |
| `vx:tex.detile` | `texture.rs:Staging::detile`, `fill_plane` | Tempo de CPU em `vx:tex.detile_cpu_us_per_frame`. A transferência correspondente aparece como `vx:tex.copy` no GPU Trace. |
| `vx:present.compose`, `vx:present.copy`, `vx:present.overlay` | `compose.rs:Composer::record`; `host.rs:Host::present_planes` | Composição de planos, cópia para o swap chain e overlay do Victory |
| `vx:barrier.coh.send`, `vx:barrier.coh.compare`, `vx:barrier.card.steps`, `vx:barrier.prefetch` | `host.rs:Host::card_before`; `host/send.rs:Own::record`; `host/upcheck.rs:record`; `host/card_steps.rs:record`; `host/prefetch.rs:record` | Barreiras dos passes de coerência |
| `vx:barrier.place`, `vx:barrier.indirect`, `vx:barrier.ambient` | `host/place.rs:wait_for_all`, `HostList::place_copy`, `place_source`, `placed_constants`; `indirect.rs:record_clamp_as`; `indirect/batch.rs:record`; `host.rs:HostList::zero_card_budget`, `card_budget_out`, `batches_common`, `indirect_common`; `ambient.rs:uav_barrier` | Sincronização de cópias posicionais, scratch de indiretos e UAV do ambient |
| `vx:barrier.prelude`, `vx:barrier.targets`, `vx:barrier.tex.alias`, `vx:barrier.tex.copy`, `vx:barrier.tex.state` | `host.rs:HostList::prelude`, `HostList::settle`, `record_twin_copy`; `draw.rs:State::set_targets`; `list.rs:ensure_state`, `clear_rtv`; `texture.rs:alias`, `record_upload` | Transições e aliasing espontâneos de texturas e targets |
| `vx:barrier.present` | `host.rs:Host::present_planes`; `host/menu.rs:Menu::draw`; `host/panel.rs:Buffers::copy` | Estados do swap chain e dos buffers do overlay |

## Linha `rf-x2:` e interpretação

- `page_faults_sec` é a diferença de `PageFaultCount` de `GetProcessMemoryInfo` dividida pelo tempo real. Inclui faltas leves e pesadas. `working_set_bytes` e `private_bytes` vêm de `WorkingSetSize` e `PrivateUsage`. `system_commit_bytes` vem de `GetPerformanceInfo` e inclui o compromisso do sistema inteiro.
- `system_pages_input_sec` é `\Memory\Pages Input/sec` via PDH, amostrado na mesma cadência. Mede páginas lidas do disco para resolver faltas pesadas no sistema, incluindo arquivos mapeados e o pagefile. Pode vir indisponível. Compare com os faults do processo e o working set antes de atribuir a leitura ao Victory.
- Com as estatísticas ligadas, as barreiras são contadas por descritor gravado, inclusive os lotes com vários tipos. `global_barrier_per_frame` cobre a variante enhanced do caminho posicional. As listas são contadas quando entregues ao `ExecuteCommandLists`, inclusive quando saem da fila de espera do Present em `host/shown.rs:run_on`. Os campos `*_per_frame` são médias por quadro iniciado pelo título entre duas linhas, cerca de cinco segundos. Listas gravadas em um quadro podem ser enviadas em outro.
- Os ranges aumentam dados de gravação e captura. A opção 2 pode alterar mais a cadência de CPU e de submissão do que a opção 1. Compare com uma captura da mesma cena sem ranges. Os contadores de barreira cobrem os pontos instrumentados desta tabela e os comandos do título traduzidos em `list.rs:barrier`, `sync` e `barrier_groups`. Caminhos fora da instrumentação, incluindo código do driver, podem contribuir para o tempo restante.
- Com ambos os switches desligados ficam apenas as guardas locais. A instrumentação usa as listas existentes e mantém a sequência de comandos, as barreiras e as submissões do renderizador.

## Testes unitários no Windows

```powershell
cargo test -p victory-gpu --lib rf_x2::tests
```

Os testes cobrem nomes estáveis, granularidade dos trechos do título, classificação dos descritores de barreira e deltas de contadores com wraparound.
