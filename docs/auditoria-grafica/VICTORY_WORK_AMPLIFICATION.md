# Quanto trabalho chega à fila

**COMPROVADA — amplificação de submissões e listas na janela observada.** A captura tem 1.140 submissões do título e 1.713 execuções host nos pontos instrumentados nos tokens interiores, **1,50×**. As 13.335 entradas de listas do título correspondem em volume a 19.622 entradas de listas executadas, **1,47×**. São razões de volumes no intervalo, sujeitas a fluxo atravessando as bordas. Não atribuem a cada lista guest exatamente 1,47 listas host. A frequência dessas operações também não mede seu custo.

Fonte primária: `%TEMP%/victory-frame-ab-751a4b55ed9b4a3baa74f3590185946f/render-trace.tsv`. Contar tokens 3407–3463 inclusive, unir execute_list a execute por batch (execute_list não carrega frame). Exemplos nas linhas 6, 24–25, 111, 275, 1594–1595, 1685, 3525, 6055. Implementação em `crates/victory-gpu/src/host/render_trace.rs:165` e `host/gpu_time.rs:501–514`. Todas as 1.736 linhas execute da captura completa têm tipo DIRECT. A fila COPY auxiliar executa listas diretamente em host.rs:338–345 e fica fora desse contador. As razões de volume valem para os pontos instrumentados, não para todas as filas físicas. Há 42 eventos de perda no arquivo inteiro, incluindo 41 reutilizações antes de fechar. Treze eventos de perda carregam tokens do recorte interior; esse token pode diferir do token do comando perdido. A medição cobre um processo e uma cena.

|Categoria e unidade|Entrada guest|Saída host na janela de 57 tokens|Fator ou interpretação|
|---|---|---|---|
|Submissões / referências a listas|1.140 / 13.335|1.713 / 19.622|1,50× / 1,47×; arquitetura de preparação, coerência e publicação|
|Draw / DrawIndexed diretos|sem contador pareado|8.066 / 6.758 marcas|desconhecido|
|DispatchMesh direto|sem contador pareado|111.835 marcas|desconhecido|
|ExecuteIndirect|sem contador pareado|358.439 marcas|cada marca pode consumir vários registros, inclusive zero|
|Dispatch direto|sem contador pareado|105.816 marcas|não inclui todos os kernels internos e indiretos|
|Par mesh/pixel quente|sem grupo guest independente|33.573 marcas DispatchMesh; mediana de 26.482 grupos solicitados por token via produto XYZ|frequência real separada de custo|
|Invocações, primitivas, vértices, pixels|sem estatísticas Xbox|sem pipeline statistics por estágio|desconhecido; dimensões do viewport não medem cobertura|
|Instruções, loads, stores, atomics|ISA estática diferente de execução dinâmica|faltam contadores físicos por programa|ver auditoria do compilador; razão de instruções heterogêneas não mede eficiência|
|Barreiras|pedidos de ordenação globais|692,9 gravados e 111,2 suprimidos por quadro na janela do log 16932:26433|0,86× para esse caminho específico (692,9/804,1); outros tipos fora do denominador|
|Transições de textura|origem não emparelhada|418 por quadro no mesmo log|trabalho explícito da representação host|
|Alocações e descritores|objetos e writes guest sem identidade pareada|recursos físicos e views derivados; ver relatório de memória|um objeto pode ter várias views; fator desconhecido|
|Uploads / cópias|sem byte guest comparável|655,8 runs de envio calculados, 259,5 gravados por quadro; 29,4 MB/quadro em outro intervalo|0,40× após agregação entre duas fases host; não é host/guest|
|Readbacks|sem contador de pedidos comparável|1.050 eventos relay_readback; cada evento trata submissão, não bytes|desconhecido|
|Esperas|dependências do título parcialmente visíveis|12,6 ms por quadro de espera da thread de submissão no log 16932:26378|tempo potencialmente simultâneo ao trabalho GPU|
|Passes explicitamente adicionais|nenhum comando guest homônimo demonstrado|10.701 marcas de clamp em lote e 813 de prefetch; 57 DLSS|contagens absolutas; dividir por zero seria indefinido|

**COMPROVADA — o custo nomeado pelo pipeline e a frequência do pipeline são medidas diferentes.** O resumo por janela registra ~28 passes/quadro cujo primeiro shader é o par caro, enquanto o rastreio fino registra mediana de 589 comandos com esse par por token interior, incluindo intervalos de duração zero. O script local `target/summarize-victory-render-trace.py:17–31` exclui durações zero. Seus 20.865 comandos com tempo positivo na captura inteira não são um censo de comandos. Atribuir ~14 ms a 28 invocações de shader daria uma resposta fisicamente inválida.

**DESCONHECIDA — excesso de pixels ou geometria.** Há 62.608 eventos draw_not_recorded na janela. A linha 111 exemplifica uma tentativa ExecuteIndirect sem motivo no evento. O contador não distingue descarte legítimo, pipeline ausente, tentativa vazia e falha de tradução. Comparar com captura guest no mesmo save e câmera, inclusive comandos descartados e contagem efetivamente consumida de cada buffer indireto. Para instrumentação futura, associar ID estável de comando guest, motivo de descarte, IDs de comandos host, bytes e subresources, fila e fence. Isso permitiria separar fan-out de duplicação e listar kernels internos por produtor. [Comandos indiretos D3D12](https://learn.microsoft.com/en-us/windows/win32/direct3d12/indirect-drawing-and-gpu-culling-)
## Contagem geométrica no pipeline caro

O MS f0c7efd2aceb2aaa reemitido pelo binário release chama SetMeshOutputCounts(128,128) e inicializa os índices de cada primitiva como degenerados no prólogo. A captura registra mediana de 26.482 grupos diretos por token para o par, o que implica 3.389.696 slots declarados de cada tipo nesse cruzamento de evidências. O cálculo dá volume lógico previsto, sujeito à correspondência offline/runtime. A distribuição da contagem útil guest em MSG_GS_ALLOC_REQ falta. Portanto o fator de expansão geométrica e o número de primitivas rasterizadas continuam desconhecidos. Ver [medição do compilador](VICTORY_SHADER_COMPILER_AUDIT.md).