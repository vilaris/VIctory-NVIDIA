# X6 · Fidelidade gráfica

## Manchas nos braços

Há duas falhas independentes relevantes para a cena. A causa visual de cada
mancha precisa de A/B, porque a captura disponível tem as duas condições.

- Com o X4 ligado, `PICK_GATE` cancela tanto o temporizador de heap quanto os
  pedidos da fila late no submit do título e depois de um corte. A submissão
  ainda espera uma rodada quando uma lista carrega o bit de acesso ao heap.
  Essa rodada chega depois do `heap_buffers` e do `early_textures`. O primeiro
  pode já ter fixado a seleção de buffers do quadro e o segundo pode já ter
  consultado uma fila de texturas incompleta. Aquecer o heap uma única vez na
  primeira ativação de `PICKED` deixa esse problema nos quadros seguintes.
- A prova do X4 via `Translated::used` identificava o root de tabela e o
  deslocamento do shader guest. Um handle do DXIL ainda podia sair do heap
  mutável, por exemplo quando a tabela é direta ou seu snapshot não pode
  ser criado. A procedência também agrupava todas as views ou todos os
  samplers pelo tipo de heap. Um leitor descartado dessa forma podia deixar
  tanto a lista sem bit quanto o heap fora da rodada.
- O log `victory-nvidia-play-20261128-004001-31668.log` também tem duas falhas de
  tradução de compute de skinning com o X4 ligado. As saídas desses produtores
  podem afetar geometria e atributos da pele se os dispatches forem pedidos.
  O log de tradução na carga, sozinho, não mede quantos dispatches foram
  descartados na cena. O histórico de GI é outra variável independente.

O X6 usa os bits de indexação de views e samplers do `SFI0` emitido e cobre os
dois SRVs de strides. Heap emprestado, tabela com binding ausente e metadados
indisponíveis recebem cobertura completa. A primeira passagem de descoberta de
buffers e a primeira passagem antecipada de texturas pedem rodadas antes de ler
seu estado, limitadas a uma por heap e quadro para cada passagem. As rodadas
no envio continuam cobrindo listas marcadas, inclusive listas cortadas ou
reenviadas. O marcador acompanha o HostList gravado. O filtro continua podendo
omitir heaps sem consumidores identificados e o comparador por blocos continua
ativo. O ganho pode cair em cenas com muitas tabelas diretas e o limite de uma
passagem de buffers por quadro continua sendo uma aproximação.

`rf-x4 diverge` traz os words, a chave publicada e a última leitura direta de
tabela registrada para aquele slot, quando disponível. Para índice calculado
na GPU, traz até 16 pares recentes de shader e PSO que puderam acessar o heap.
Cada entrada contém chave do PSO, hash e nome do catálogo. O índice efetivo
escolhido em execução exige feedback da GPU. O verificador também preserva
leituras que falharam ou que encontraram uma escrita concorrente para outra
rodada.

## Pipelines descartados e sombras

Com `VICTORY_RF_X6_STATS=1`, `rf-x6: pipeline activity` registra a operação,
a chave do PSO, os hashes e nomes disponíveis, a razão e o total por quadro na
janela de FPS. A instrumentação está disponível em release. Ela separa
tradução que falhou, PSO recusado, PSO pendente, pipeline ausente, exclusões e
erros de argumentos indiretos. Uma pipeline pode ficar pronta entre a consulta
e a contagem, caso reportado separadamente. A linha antiga mistura famílias no
contador de exclusões. Por exemplo, a janela de 00:41:17 registrou 134
descartes diretos e 801 indiretos, com 802 exclusões intencionais no total.
Esse log agrega as causas, então os valores por PSO precisam de uma nova
captura.

Os nomes abaixo vêm dos containers em `known-shaders.bin`. O catálogo inclui
variantes que a cena pode nunca executar.

| Hash | Entrada | Classe e estado |
| --- | --- | --- |
| `5e3dd99e0f067aee` | `CS_Emit_Dynamic_Model_AttachMesh_Skinned` | Skinning, falha no lowering wave64 → wave32. |
| `ae0deeafe219c15c` | `CS_Emit_Dynamic_Model_Skinned` | Skinning, mesma falha. |
| `6b82bc7d22016cb5` | `CS_CollectTriangles_Meshlets` | Triângulos para decal. O grupo de 1.024 threads usa os 32 KiB de LDS antes dos encontros de waves. Corrigir exige outra estratégia de lowering. |
| `926c46bab3b86bd6` | `CS_TransmissionVolumetricsSubmerged` | Luz volumétrica submersa. O digest já está na lista wave-wide de `4f49709`. A falha persistiu na captura e precisa de `VICTORY_WAVE_WHY=1` para atribuir a etapa exata. |
| `02384a60fda6f2d0` | `CS_DefragHeap_ComputeRelocations` | Decais, exclusão por custo. O commit `672442b` registra 11,85 ms GPU por quadro. `89cfae2` reduz os encontros de waves e já está na base. |
| `731a39171b568298` | `CS_IndirectApplyDiffusionInScattering_HeatHaze` | Névoa, luz no ar e distorção térmica. `41b4a05` relaciona a profundidade transparente indevida aos blocos em contornos e introduz o teste de profundidade antecipado. |
| 1.000 variantes, ex. `29be03ccaa4e353a` | `CS_RaytracingResolve*` | Resolve de hits RT, omitido pela política sem builder. |
| `39f0e9aeb156a61c` | `CS_ScreenProbes_ActivateRays` | Ativação de sondas GI. O PSO host aparece no log. A versão desta base executa com os padrões e exclui se `VICTORY_ACTIVATE_RAYS=0`. |
| `be6ce5d849d51127` | `CS_ShadowRenderTiledCascadesCB` | Sombra em cascata por tile. PSO criado e ~1 dispatch/quadro registrado, com ~1,2–1,6 ms GPU nas janelas em gameplay. |
| `191ecaed66eea3b1`, `ab5455fc6abb804f`, `922e1d0ee1e19a85`, `ce79e9eba7ae0348`, `20b78ee12ecdb57d`, `d7db57731ada919b`, `f133e35efed2a48a` | `CS_ShadowRenderTiledCascades` e variantes Alpha, CB e PPPC | Cascatas raster no catálogo. Os dois hashes de `LEAVE_OUT` pertencem a outras famílias. |
| `9991e29feaa26e3e`, `1580e5a72e54fcdf`, `0c028454b58543f0`, `eab96ed5d1684967` | `CS_ShadowTileClassify`, `CS_ShadowTileSimpleClear`, `CS_ShadowUnifiedClearRaster`, `CS_ShadowUnifiedClearRT` | Classificação e limpeza de sombras. A captura não fornece a contagem efetiva de argumentos por variante. |

A ausência de jobs de criação na fila é compatível com falhas permanentes de
tradução. Os dois digests de skinning estão ausentes do `scene_kernel` desta
base. O commit `2b028ea`, fora da ancestralidade do branch, registra sucesso
em RTX 4070 ao incluir esses digests na lista de computes de 64 threads com
retry wave-wide. Ele também cobre `57f1b81de55c2842` (edge hash de modelo
dinâmico) e `a10179ab7eda0dc3` (volume de portais), cujo digest já consta
na base. Reaplicar somente os digests faltantes preserva o limite de 64
threads e as condições do tradutor.

Para cascatas, o clamp compute lê a contagem da GPU. Ele pode zerar grupos
quando a contagem excede o limite ou o orçamento do lote. A telemetria nova
mostra rejeições anteriores ao clamp e aceitação do comando de cascata CB.
Tempo GPU e aceitação não fornecem os três dwords de cada dispatch. Um
readback dos argumentos originais e limitados na mesma lista seria necessário
para atribuir zeramentos esporádicos.

`VICTORY_RF_X6_RESTORE_LEFTOUT=1` libera esses dois hashes mesmo se estiverem
nas configurações. A névoa só é liberada com `VICTORY_EARLY_DEPTH=1`.
A restauração de decais pode aumentar o tempo GPU e depende também do
coletor de triângulos. O histórico encontrado documenta custo e defeito visual
para esses dois casos. Um TDR específico desses hashes fica sem evidência.

## GI com RT desligado

O relato do usuário põe o builder em ~3 fps com imagem incorreta. O estudo
W4, recuperado pelo commit `1a5ffb7`, descreve UAVs conservando os últimos
bytes quando o produtor sai do fluxo. A versão atual já mantém a ativação de
sondas ligada por padrão por causa de resíduos roxos observados quando omitida.
O resolve de hits RT continua omitido. Os misses de reflexão ainda podem
receber o céu. `VICTORY_AMBIENT` mistura a estimativa de luz com o resolve de
sondas, `_AIR` limita a luz espalhada e `_SPECULAR` substitui contribuições
de céu nos misses antes do resolve screen-space.

`VICTORY_RF_X6_RT_FALLBACK=1` reutiliza esse passe de ambient para substituir
a saída difusa conhecida de `CS_ScreenProbes_TemporalResolve` por uma
estimativa corrente. Usa o nível configurado em `VICTORY_AMBIENT`, o padrão
experimental 0,8 quando ausente e uma contribuição de partida 0,02 quando
o estimador ainda não guardou radiância. O passe usa o mesmo UAV tipado e as
barreiras do ambient. A contribuição de fallback é excluída do histórico do
estimador após a primeira amostra. Essa intervenção ocorre na fronteira da
saída difusa. Os UAVs intermediários também podem carregar hits screen-space,
máscaras ou listas de sondas para outros consumidores. Limpar esses recursos
requer provar o layout, o sub-recurso e o valor neutro por variante. O custo
adicional é o do ambient que já roda na configuração do usuário. Preservar
somente a estimativa pode remover parte da luz de céu ou de hits legítimos
do resolve difuso. Reflexos e ar seguem seus controles existentes.

## Roteiro de validação

Use o mesmo save, enquadramento, preset e tempo de aquecimento. Edite
`%LOCALAPPDATA%\victory\victory-settings.txt` e reinicie entre execuções.
Mantenha `VICTORY_BVH_BUILDER=0`, `VICTORY_RF_X6_STATS=1`,
`VICTORY_RF_X4_STATS=1` e os valores atuais dos três controles de ambient.
Comece com `VICTORY_RF_X6_RESTORE_LEFTOUT=0`.

| Rodada | `VICTORY_RF_X4_PICK_GATE` | `VICTORY_RF_X4_PICKED` | `VICTORY_RF_X4_VERIFY` | `VICTORY_RF_X6_RT_FALLBACK` |
| --- | --- | --- | --- | --- |
| A, referência | 0 | 0 | 0 | 0 |
| B, descritores | 1 | 1 | 1 | 0 |
| C, GI | 0 | 0 | 0 | 1 |
| D, custo combinado | 1 | 1 | 0 | 1 |

Defeito só em B aponta para a rota X4. Blocos que respondem a C com o X4
desligado apontam para a contribuição das sondas. Confira também os descartes
de skinning na mesma janela de B e a cor dos braços em movimento. Compare
`rf-x4 diverge`, `rf-x6:`, tempo de preparação, tempo GPU e sombras. Em
uma passada separada, veja o custo e a imagem com
`VICTORY_RF_X6_RESTORE_LEFTOUT=1` e `VICTORY_EARLY_DEPTH=1`.
Esta entrega foi verificada por leitura estática e `git diff --check`.
A tarefa reservou compilação, testes e jogo para depois.
