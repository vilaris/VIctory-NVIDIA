# Caminho crítico e orçamento de 33,33 ms

**FORTEMENTE SUSTENTADA.** As listas temporizadas ocupam a maior parte do intervalo observado. Em `%LOCALAPPDATA%/victory/logs/victory-nvidia-play-20261127-203447-7572.log:26527–26529`, 327,8 listas/quadro ocupam 72,32 ms de uma extensão de 76,2 ms. Há 3,90 ms entre listas. O detalhamento da mesma janela dá 44,05 ms de draw, 19,18 ms de compute, 0,64 ms de steps e 8,45 ms classificados como próprios. As listas integralmente próprias somam 7,93 ms, uma definição mais restrita. Outra janela, linhas 26603–26605, dá 76,84, 46,59, 19,89 e 9,62 ms respectivamente para união, draw, compute e próprios. A divergência entre janelas limita conclusões sobre um frame individual.

## O que os relógios medem

- `crates/victory-gpu/src/host.rs:10549–10564`, Meter::frame, mede diferença entre duas chamadas da CPU no caminho de apresentação. FPS renderizado médio é número de intervalos dividido pela soma dos intervalos. 1000/78,5 = 12,74 é o inverso de um intervalo típico. Frame generation e refresh do monitor exigem medidas separadas.
- `host/gpu_time.rs:10–37,47–61,476–495,519–558`, collect/log, usa EndQuery, resolve, coleta tardia, ordenação e união de spans, divididas pelo número de frames da janela de logging. Não mede ciclos ativos dos SMs. Listas que aterrissam tarde cruzam a janela; spans podem incluir stalls. Um intervalo livre entre listas não identifica quem atrasou o próximo submit.
- O primeiro pipeline dá nome ao passe de draw; limite de 62 marcas de passe/lista e 2.048/frame fora do modo detalhado. Assim, 14,11 ms rotulados f0c7efd2aceb2aaa / 7d85c9cab3c80721 (log 201325-16932:26366) sustentam o ranking de **passe**. Em outra janela o mesmo rótulo chega a 18,21 ms. O recurso caro pode estar em mesh, pixel, rasterização ou memória. Um tempo por invocação física permanece desconhecido.
- Na captura TSV descrita na matriz, mediana por token interior de intervalos associados ao par de 15,52 ms. O relógio é granular e os comandos podem se sobrepor internamente. Não se devem somar outras janelas nem atribuir latência isolada a cada grupo.

## Verificação dos A/B e sensibilidade à seleção

Recálculo independente dos logs de 27/11, janela fixa [120,150) segundos a partir da primeira linha ISO de cada log, sem filtro de draw ou foco. P99 por interpolação linear (quantiles inclusive), FPS = 1000/média em ms. Comparabilidade de cena exata e associação binário→Git incompletas. As condições estão na linha 1 de cada arquivo. Este recorte é reproduzível; não identifica a janela histórica exata da solicitação.

|Log, linhas|Quadros|P50 ms|P99 ms|FPS|Mediana das janelas de união GPU ms|
|---|---:|---:|---:|---:|---:|
|201325-16932, 23752–25582; section=2|353|81,58|155,48|11,77|70,52|
|203447-7572, 23974–25863; memória amostrada|354|82,23|151,68|11,80|70,90|
|204950-3624, 24056–25876; mesh variante|309|94,74|166,40|10,31|82,76|
|202808-6208, 24236–26329; async=plan|213|126,10|304,33|7,09|123,08|

O log 203934-25432 tem strict_retirement=1 **e** async_queue=plan; termina antes de 120 s. A linha 1 de 202808-6208 traz async_queue=plan, sem flag explícita de strict retirement. Sem provar defaults desse binário ou identificar a série histórica, a coincidência de 126,10 ms é insuficiente para atribuir esse resultado à liberação estrita. Também não reproduzimos P99 338,9 ou 99–100 nesse protocolo. A variante mesh apresenta intervalos de apresentação e união GPU maiores nesse recorte. 201325 tem SECTION_MB=2 e omite TEXTURE_SHORT_PIN. Os demais ensaios da tabela têm SECTION_MB=0 e SHORT_PIN=0. O comparador mais próximo, 203447, ativa MEMORY_SAMPLES=1 e usa outro binário. 202808 usa o executável frametime-validation. Essas diferenças e a variação de cena impedem isolar causalidade. Hardware consultado nesta sessão via CIM/nvidia-smi: Ryzen 7 5700X, 31,93 GiB disponíveis como memória física instalada visível ao SO, RTX 4070 12.282 MiB, driver atual 621.01. O driver histórico precisa de confirmação própria.

## Decomposição fina de cinco segundos

Na captura TSV (tokens 3407–3463), para cada gpu_command válido considerar (end-start)/hz e o nome do pipeline, incluindo dispatch indireto de compute como compute. Medianas por token: raster direto/indireto 42,40 ms, compute identificado 15,35 ms, host_pass ou nome iniciado por “ours:” 13,73 ms, demais passos 1,23 ms. A categoria host_pass inclui mudança de RT e bibliotecas, portanto difere de OURS no resumo normal. Tokens são atribuídos na gravação. As medianas não formam caminho crítico por adição.

|Origem e operação|Marcas nos 57 tokens|Média de intervalo atribuído por token|
|---|---:|---:|
|Coerência, upload + hash (início de lista)|665|6,232 ms|
|Clamp de argumentos indiretos, marca de batch|10.701|1,688 ms|
|Prefetch dos pedaços marcados|813|1,583 ms|
|Comparação após listas|813|1,430 ms|
|DLSS Super Resolution|57|1,204 ms|
|Ambient gather / raise|57 cada|0,338 / 0,327 ms|
|Depth twins no final da lista|171|0,205 ms|
|Compute 8f9eb68d0c6dd5cf / 34fb51b8a0312e82|540 / 57|0,662 / 0,609 ms|
|Compute indireto 2dc364df5d3278b8 / be6ce5d849d51127 / 915ed81ea580a276|57 cada|0,551 / 0,526 / 0,501 ms|

Marcas são intervalos entre timestamps e podem envolver várias operações. Upload, hash, compare e prefetch têm dependência de coerência. Clamp precede consumo do argumento. A semântica específica de cada compute do título exige associação a recursos lidos/escritos; hash não identifica um algoritmo. A linha DLSS registra 1484×835 → 2560×1440. Isso determina os argumentos do upscaler naquele passe; resolução dos demais alvos, samples cobertos, MSAA e overdraw demandam inspeção individual.

## Captura física necessária

Não foi localizado ngfx no PATH, instalação de Nsight Graphics/PIX nas pastas usuais nem captura física correspondente ao TSV. WPR e nvidia-smi estão presentes. Não houve replay do jogo nem alteração das opções. Atribuição a ALU, cache, ROP, spills ou baixa ocupação permanece **DESCONHECIDA**.

1. Em PIX Timing Capture, executar baseline e variante com mesmos binários identificados por SHA-256, save, câmera, foco, clocks e recursos gráficos, registrar pelo menos três repetições em ordem alternada, separar partida fria e aquecida. Correlacionar Present do app com DIRECT/COMPUTE/COPY, signals/waits, CPU submit, ETW de I/O, compilação, fences e VidMm; distinguir frames sintetizados. Traçar predecessores do present e folga de cada dependência.
2. Em GPU Capture, localizar os hashes pelos PSOs, exportar DXIL e estado dos alvos, viewport/scissor, depth/stencil, blend, sample count e tamanhos de recursos. Comparar conteúdo das argument/count buffers e estatísticas de MS/PS, primitivas e clipping para os mesmos draws nas variantes. Registrar eventos com predicação e clears/resolves.
3. Em Nsight Graphics, GPU Trace e Shader Profiler nos mesmos ranges, recolher throughput SM, TEX e ROP, warp occupancy, active/eligible warps, stalls, registradores/thread e local load/store, memória compartilhada e barreiras, L1/L2 e DRAM, contagens de exportação e instruções por PC quando expostas. Comparar shaders e outputs separadamente. Capturar clocks e verificar perturbação do replay. Sem esses dados não há diagnóstico físico entre spilling, latência, distribuição de primitivas e exportação. [Nsight](https://docs.nvidia.com/nsight-graphics/) · [PIX](https://devblogs.microsoft.com/pix/documentation/)

Mesmo no contrafactual irreal de retirar integralmente os 8,45 ms próprios da janela de 72,32 ms, restariam aproximadamente 63,87 ms de intervalos. O déficit perante 33,33 ms seguiria superior a 30 ms. Isso delimita o tamanho do problema; não projeta FPS após uma intervenção.