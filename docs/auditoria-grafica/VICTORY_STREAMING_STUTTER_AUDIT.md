# Auditoria de streaming e stutter do Victory

Código de referência `24e4014`. Log primário `%LOCALAPPDATA%/victory/logs/victory-nvidia-play-20261127-204334-12000.log`. As linhas 1 e 5 identificam o ensaio com Series X, RTX 4070, DLSS Balanced, FG 2× e executável `target/refresh-batch-ab/victory.exe`. As métricas caracterizam esse ensaio. A configuração do commit é analisada no código fixado.

## O que a sessão mostra

| Hora UTC e linha | Observação medida |
|---|---|
| 20:44:12, linhas 8.071–8.089 | Abertura de `core_restofworld001.rpf`, `core_restofworld.rpf` e arquivos associados. É um marco observável de entrada no mundo. |
| 20:44:40, linha 13.946 | 13.420 slots na fila, 6.613 prontos para serviço e 7.057 já carregados esperando revisão. |
| 20:44:45, linha 14.287 | 24.594 slots na fila, 321 prontos e 24.277 já carregados esperando revisão. |
| 20:44:53, linhas 20.062–20.063 | 25.370 slots, zero prontos no instante da amostra. A contabilidade de textura registra 8.283 MB contra o limite de 7.979 MB, 80% de um orçamento de 9.974 MB, e adia novas texturas. |
| 20:45:03, linhas 21.444–21.445 | 29.794 slots, 113 prontos e 24.089 carregados esperando revisão. Uso contabilizado de 8.539 MB contra 7.986 MB. |
| 20:46:16, linhas 27.112–27.123 | 9.456 slots, 25 prontos. Uso contabilizado de 8.952 MB contra 8.014 MB. Janela de desempenho em 83,9 ms/frame, com 41,5 ms de preparação, dos quais 17,3 ms em cópias, 7,7 ms de GPU e 6,7 ms de cópia de volta. |
| 20:46:19, linhas 27.305, 27.318–27.319 | O contador de atualizações de texturas já presentes chega a 39.500 sob pressão. O frame 4096 é apresentado. O contador acumulado de shaders mostra 4.070 hits e 1 miss. |

Entre as linhas 21.520 e 27.313, as 913 amostras `rendered frame elapsed_us` têm P50 de 73,795 ms, P95 de 124,933 ms, P99 de 155,583 ms e máximo de 229,235 ms (percentil nearest-rank). Esses eventos medem o intervalo entre chamadas de `Meter::frame` (`host.rs:10549–10564`), e a geração de frames está habilitada na sessão. Os números servem para caracterizar a cadência dos frames renderizados registrados, com apresentações geradas e intervalo de exibição efetivo sem medição independente. A janela coincide com **9,306 s acumulados** em 11.937 pares de residência/mapeamento de UAV local. [Auditoria de memória](VICTORY_MEMORY_AUDIT.md)

**COMPROVADA por medição.** A política de espaço adiou texturas novas enquanto atualizações de texturas residentes e apresentação continuaram. A contagem total da fila mistura trabalho pronto, rechecagens e outros estados. O uso efetivo da política inclui crédito de blocos e reservas, e difere da capacidade nominal da placa.

**DESCONHECIDO.** Quanto cada fonte causou individualmente no P99 e quanto da janela foi travada especificamente por I/O, decodificação, residência, publicação de texturas, compilação de PSO ou espera da submissão. A sobreposição é possível. Os hits de shader contam outra camada de cache que o PSO do driver. Nenhuma captura `.etl` foi localizada na busca recursiva em `%TEMP%`. A análise usa telemetria de aplicação e código.

## Caminho de dados e pontos de espera

| Etapa | Implementação no commit | Consequência |
|---|---|---|
| DirectStorage do título | `crates/victory-hle/src/modules/dstorage.rs:239,329,647,717,805,1072` | Workers executam I/O e descompressão no caminho emulado. `VICTORY_DSTORAGE_LOG` e `VICTORY_DSTORAGE_TRACE` atribuem filas, pedidos e atividade de leitura/decode. |
| Páginas guest | `crates/victory-hle/src/modules/xmem.rs:827,979,1077,2135`, `crates/victory-gpu/src/texture.rs:2072` | Mapeamento e escrita podem deixar a view disponível antes de seu conteúdo completo. Falta de páginas leva o passe a adiar ou aceitar níveis disponíveis, conforme o caso. |
| Adoção das texturas pelo host | `draw.rs:2915,3138,3370`, `descriptor/host.rs:2358` | `heap_textures` percorre slots prontos no caminho de gravação, reconhece as páginas disponíveis, busca/cria textura e prepara upload. Existe passe antecipado e caminho específico de bake. |
| Staging, upload e publicação | `texture.rs:2137,2195`, `texture/ahead.rs:115`, `descriptor/host.rs:314,3120` | Upload adiantado é limitado e depende da elegibilidade. Slots podem ficar com textura neutra até concluir o upload. Um bake que precisa do recurso pode executar lista interna de forma síncrona (`draw.rs:2998`). |
| Admissão de memória | `resource.rs:1819,2016,2033`, `residency.rs:74,206`, `draw.rs:3302–3334` | Orçamento da placa, pressão de RAM/commit e staging limitam novos uploads. O caminho existente pode continuar atualizando texturas que já têm espaço. |

As referências abreviadas da tabela pertencem a `crates/victory-gpu/src`. `texture/ledger.rs:423` expõe a indicação de carregamento observada pelo runtime.

## Diferença em relação à investigação anterior

Em `24e4014`, o launcher oferece `VICTORY_TEXTURE_CATCHUP=1`, `VICTORY_TEXTURE_STAGE_MS=16` e `VICTORY_LEVELS_ON_DEMAND=0` (`apps/victory/src/play.rs:54–56`). O passe aplica 3× o orçamento de bytes e um piso de 200 ms quando há pelo menos 1.024 slots prontos e catch-up ligado. O caminho especulativo de carregamento permite 4× bytes adicionais e piso de 120 ms, sujeito à admissão de RAM e VRAM (`draw.rs:3287–3334`). As rechecagens combinam limite em frames e teto aproximado baseado no tempo por frame (`draw.rs:839–849`). A constante de 200 ms está embutida nesse commit, apesar do nome de variável de ambiente no comentário.

O ensaio histórico `play-20261127-094655-17644.log` do relatório de streaming usava outra configuração, com catch-up desativado. Serve para documentar a fila vinculada ao ritmo do frame e o custo de detile/publicação na época. Seu diagnóstico sobre pressão de VRAM diverge do relatório de memória para o mesmo log. O ensaio 204334 documenta diretamente a pressão local mediante a linha de orçamento, e os dados do ensaio antigo não determinam a participação de I/O nesta sessão. [Streaming histórico](https://github.com/vilaris/VIctory-NVIDIA/blob/24e4014e9f634dd07124c1bdd7f17ee7b1e04197/docs/performance/AGENT_REPORTS/05-streaming-preload.md) · [Memória histórica](https://github.com/vilaris/VIctory-NVIDIA/blob/24e4014e9f634dd07124c1bdd7f17ee7b1e04197/docs/performance/AGENT_REPORTS/02-memory.md)

## Como atribuir os stalls e validar mudanças

1. Repetir o mesmo save e trajeto com estado de cache registrado, separando partidas frias e aquecidas. Registrar presents, thread e pilha de espera, frames em voo, GPU queues/fences, budget/residência, file I/O e compilação. Correlacionar com `VICTORY_DSTORAGE_LOG`, amostras de residência, fila de slots e eventos do Victory em QPC.
2. Carimbar o caminho request → leitura → decode → escrita guest → view/slot → staging → upload fence → atualização do descriptor → primeira amostragem por apresentação. Medir profundidade, idade dos slots prontos e visibilidade da textura neutra. As aberturas dos pacotes podem marcar transições, mas a view e as páginas concretas determinam se o recurso pode ser preparado.
3. Testar, isoladamente, reaproveitamento de UAVs, admissão de texturas e trabalho de adoção fora da gravação. Comparar P95/P99, idade de slots prontos, consumo de RAM/VRAM, frames com placeholder e estabilidade. Subir a cadência do passe pode drenar mais slots quando há espaço e custar tempo na gravação. A relação entre tempo de CPU e qualidade visual exige as duas métricas.

[Referências oficiais](VICTORY_OFFICIAL_REFERENCES.md) · [Caminho crítico](VICTORY_GPU_CRITICAL_PATH.md)


## Codecs, conteúdo reaproveitável e primeiro consumo

**COMPROVADA no caminho implementado.** dstorage.rs:993,1049–1052,1415–1466 decodifica flags por requisição: dados crus, zlib, BCPack ou zlib seguido de BCPack. O caminho usa zlib_rs na CPU e bcpack::decode com tabelas mantidas pelo processo (1525–1593). O destino pode ser memória guest ou páginas físicas. O adaptador mantém trabalhadores e notificações de conclusão. A implementação expõe contabilidade de bytes e ocupação dos workers para BCPack (833–855); não foi estabelecida por asset a distribuição de codecs do frame quente. Há uma política de cache para solicitações BCPack e zlib grandes provenientes de arquivo (1483–1490). Portanto não se deve cobrar o custo de decodificação integral a cada referência a uma mesma textura.

BCPack comprime dados de textura já organizados em blocos BC. A produção dos blocos para upload e o rearranjo de layout guest para o footprint do host são etapas distintas. Não identificamos aqui uma passagem geral por RGBA seguida de recompressão BC. Para DirectStorage Windows, a descompressão GDeflate de hardware não satisfaz automaticamente os contratos de zlib e BCPack. Pedidos crus com destino e fence compatíveis são candidatos melhores para avaliar encaminhamento. Manter a ordem de conclusão e os aliases físicos que o título observa. [DirectStorage 1.1](https://devblogs.microsoft.com/directx/directstorage-1-1-now-available/) · [DirectXTex](https://github.com/microsoft/DirectXTex)

**Preparação seletiva.** O catálogo conhecido contém entradas de shader e PSO reutilizáveis. Preload prepara antes da entrada no mundo um conjunto ordenado por primeiro uso; cache persistente guarda resultado elegível entre sessões; streaming traz o conjunto efetivamente demandado; residency garante disponibilidade da memória física durante o uso pela fila. Um container disponível em disco não prova que seus bytes, descritores e layouts já estão definidos para uma vista daquele frame. O sinal para liberar gameplay requer dependências observáveis pelo jogo. A idade de slots com placeholder e a primeira fence de upload medem se essa dependência foi satisfeita. Não há evidência suficiente para responsabilizar conversão de meshes ou compilação em gameplay por um número específico de milissegundos de P99.