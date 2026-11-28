# Auditoria de memória do Victory

Referência de código `24e4014`. As linhas de código se referem a esse commit. O log principal, `%LOCALAPPDATA%/victory/logs/victory-nvidia-play-20261127-204334-12000.log`, declara Series X, RTX 4070, DLSS Balanced, frame generation 2× e `target/refresh-batch-ab/victory.exe` nas linhas 1 e 5. A medição pertence ao executável de experimento. O código fixado explica a implementação correspondente.

## Residência de buffers UAV locais

**COMPROVADA por medição.** Entre as linhas 21.520 e 27.313 inclusive, os registros `GPU residency sample` com `domain="local_uav_buffer"` cobrem 75,455 s entre o primeiro e o último relatório, frames 3181 a 4094. A soma de `resident_calls` é 11.937 e a de `mapped_calls` também é 11.937. `resident_us` totaliza 6.491.536 µs e `mapped_us` totaliza 2.814.358 µs, **9,305894 s acumulados**. Os máximos individuais são 12,822 ms e 10,169 ms, respectivamente. Ambos os contadores de erro somam zero. No log inteiro, há 19.100 chamadas de cada etapa para esse domínio e 11,958 s acumulados. Para reproduzir, filtre pelo domínio e pelas linhas indicadas e some os campos de contagem e duração. Os bytes dos contadores expressam tamanhos de alocações atendidas, com vida útil variável.

**COMPROVADA por inspeção.** `Host::uav_buffer` cria um buffer LOCAL (`D3D12_HEAP_TYPE_DEFAULT`), committed, com `ALLOW_UNORDERED_ACCESS` e `CREATE_NOT_RESIDENT` por padrão. Ao criar com sucesso, `resident_and_mapped` chama `MakeResident` e depois `mapped`. `wait_for_mapping` trava o mutex estático `ASIDE`, grava ou reutiliza uma cópia de 16 bytes em fila D3D12 COPY auxiliar, sinaliza uma fence e pode esperar indefinidamente, com o mutex ainda sob posse. O trabalho aguarda o progresso da fila auxiliar, e outros chamadores de mapeamento aguardam o mutex. A instrumentação começa antes da tomada do mutex, então `mapped_us` inclui contenção possível. A soma de 9,306 s agrega tempos dos chamadores, que podem se sobrepor. A parte que efetivamente atrasou presents permanece **DESCONHECIDA**. [host.rs:241](https://github.com/vilaris/VIctory-NVIDIA/blob/24e4014e9f634dd07124c1bdd7f17ee7b1e04197/crates/victory-gpu/src/host.rs#L241)

Âncoras no commit `24e4014` em `crates/victory-gpu/src`:

| Arquivo e linha | Evidência |
|---|---|
| `host.rs:270,292,338–345,354,381` | Medição, mutex, operação auxiliar, fence e `MakeResident` |
| `host.rs:11758–11798` | Criação do UAV local e classificação `LocalUavBuffer` |
| `host.rs:5066,8512,8593,8615`, `shadow.rs:527,672,6420`, `resource.rs:540` | Chamadores de `uav_buffer`. O agregado por domínio não identifica a contribuição de cada produtor |
| `pool.rs:686`, `resource.rs:2870`, `host.rs:11654,11743` | Outros domínios passam pela mesma proteção de residência e mapeamento |

A janela também registra 477 pares para `shared_buffer`, 41 para `writable_texture` e 28 operações de residência sem mapeamento para `depth_twin`. Essa concentração torna o domínio UAV local o primeiro candidato à atribuição por produtor.

## Segurança, falhas e deadlock

**COMPROVADA por inspeção.** O comentário de `host.rs:232–240,264–269` associa as falhas históricas n08/n11 a descritores visíveis que apontavam para novas páginas antes de o mapeamento GPU terminar. O retorno de `MakeResident` sozinho foi insuficiente no relato n11. O runtime preserva recursos até a aposentadoria das listas, inclusive com retenção extra em `keep_freed` (`host.rs:392`). `execute_sync_internal` (`host.rs:6190`) evita drenar filas do título quando uma operação interna parte de workers thin. Isso trata outra dependência de filas.

**DESCONHECIDO.** As amostras concluídas, sem erros, comprovam progresso durante a janela. Para confirmar deadlock seria necessário observar uma espera sem progresso e fechar um ciclo com pilhas de threads, donos de locks, fences, filas e respectivos produtores. A espera `INFINITE` com mutex permite que o bloqueio se propague se a fila auxiliar parar. Os contadores são publicados após o retorno da chamada e não capturam uma chamada ainda bloqueada. Um page fault de GPU ou device removal também exige evidência própria de DRED, status da fence e da fila que perdeu progresso. [Residência D3D12](https://learn.microsoft.com/en-us/windows/win32/direct3d12/residency) · [DRED](https://learn.microsoft.com/en-us/windows/win32/direct3d12/use-dred)

## Coerência guest/host e custo de declarar UAVs

`Draw::root_uav` calcula o alcance a partir do endereço até o fim do shadow (limitável por `VICTORY_ROOT_UAV_MB`), pede refresh e declara escrita quando aplicável. `tight::declare` já oferece uma declaração aprendida sob `VICTORY_TIGHT_WRITES`. `Shadow::gpu_writes` registra os intervalos pendentes. Diante de sobreposição com mudanças da CPU, `merge_under_writes` protege as escritas em voo. No retorno, `relay_land` e `card_back` detectam alterações e entregam os bytes à memória do título. O alcance da declaração afeta hash, fetch e merge. Barreiras UAV tratam ordenação de acesso a recursos. Residência, mapeamento de VA, publicação de descriptor e conclusão do landing são dependências distintas.

| Caminho | Âncora no commit |
|---|---|
| Root UAV e classificação da escrita | `draw.rs:2278–2313`, `tight.rs:228`, `shadow.rs:3837` |
| Atualização e merge CPU com escritos em voo | `shadow.rs:4363,4957` |
| Landing e cópia de volta | `host.rs:5889,6051`, `land.rs:1` |

A análise histórica do log `play-20261127-094655-17644.log` documenta cerca de 18 MB/frame lidos de volta em peças para 0,77 MB/frame efetivamente copiados como dwords, além de 9,6 ms/frame de merge na thread de submissão. São números de outra execução e medem amplificação de coerência, um mecanismo independente da contagem de criação de UAVs. [Relatório histórico, seções 1.3–1.5](https://github.com/vilaris/VIctory-NVIDIA/blob/24e4014e9f634dd07124c1bdd7f17ee7b1e04197/docs/performance/AGENT_REPORTS/02-memory.md)

## Hipóteses testáveis

1. Atribuir alocação por callsite, tamanho, duração, thread, seção/lock, fence e list ID. Comparar a trilha com os presents atrasados e separar tempo de `MakeResident`, espera no mutex e espera real da fence.
2. Testar reaproveitamento por tamanho e classe e, para recursos sem consumidores pré-publicação, agrupar preparação fora da thread crítica. Preservar endereço, alinhamento, estado de recurso, ordenação, limpeza inicial, lifetime e fence contra reutilização prematura. Publicar o slot após a confirmação do upload e do mapeamento aplicáveis.
3. Validar o efeito com A/B no mesmo cenário e com auditoria de slots/page faults sob DRED. Medir P95/P99 de presents, alocações sobreviventes, consumo de VRAM por classe e regressões de imagem/coerência. A redução esperada de waits e seu impacto em FPS são **DESCONHECIDOS** até esse teste.

Ver também [streaming e stutter](VICTORY_STREAMING_STUTTER_AUDIT.md) e [referências oficiais](VICTORY_OFFICIAL_REFERENCES.md).


## Inventário dos oito chamadores diretos de Host::uav_buffer

Todos abaixo compartilham o domínio de telemetria local_uav_buffer, recurso committed DEFAULT, ALLOW_UNORDERED_ACCESS, caminho de residência/mapeamento descrito acima. Um descritor UAV não exige um novo recurso por definição. O contador mede objetos que chegaram a MakeResident; os relatórios por domínio não registram o endereço do callsite, IDs de descritores, frees ou fences de último uso. Assim, as subdivisões têm **origem COMPROVADA** e **distribuição quantitativa DESCONHECIDA**.

|Arquivo e linha, produtor|Finalidade e tamanho solicitado|Reuso, dependência e trabalho associado|
|---|---|---|
|host.rs:5066, card_before|hashes por partes do shadow; (size/64 KiB + 2) × 16 bytes, shadow.rs:439|criado por card_make_before; acompanha comparação e readback posterior|
|host.rs:8512, make_card_budget|orçamento host para execução indireta, 256 bytes|por lista; card_budget_out copia 16 bytes das notas para buffer mapeado e transiciona estados|
|host.rs:8593, indirect_scratch|argumentos já limitados; ao menos 64 KiB, dobra e arredonda a 64 KiB|reusa capacidade entre resets; mantém buffer substituído vivo até reset|
|host.rs:8615, batch_place|8 × JOBS × SLOT por bloco; conjunto de argumentos corrigidos|buffer por lote de espaços da lista, retido entre gravações; uso como UAV e INDIRECT_ARGUMENT|
|shadow.rs:527, card_room_for|espelho local de buffer lógico no tamanho size|consumo limitado por CARD_BYTES, fallback se orçamento/criação falhar|
|shadow.rs:672, window_room_for|espelho local de janela, tamanho size|orçamento separado WINDOW_CARD_BYTES; uma faixa lógica pode coexistir com staging|
|shadow.rs:6420, by_address|cópia local de arena por endereço, len|primeiro cria staging mapeável; copia para GPU quando há espaço e mantém endereço acessível ao shader|
|resource.rs:540, twin_texture|buffer intermediário do plano stencil; bytes exatos por GetCopyableFootprints com mips/slices|existe sob condição de stencil; acompanha a textura gêmea para cópia de planos|

São mecanismos do host. Alguns materializam armazenamento necessário para um buffer guest, outros existem para coerência ou transformação de comandos. Esse inventário não sustenta que todos os 11.937 sejam temporários elimináveis, nem que sejam alocações de registradores, root constants ou buffers solicitados um a um pelo título. O primeiro experimento útil é distinguir **qual das oito linhas** domina por número, bytes e tempo em threads que alimentam a fila.

## Caminhos de endereço e grafo de locks

Em xmem.rs:1229–1253 o título troca as páginas físicas associadas a uma janela de VA; o runtime mantém livros de páginas/seções e views de endereço virtual no processo Windows. Com soft_views=copies, a visão que o título tem de um alias pode existir antes de ser materializada numa view do SO (xmem.rs:2161–2196). A VA guest identifica bytes lógicos. A GPU VA host decorre do recurso D3D12 e do offset no shadow escolhido. São identidades com lifetimes diferentes. O caminho especial de endereços crus de 40 bits usado por shaders de estruturas de raios mantém diretório por página de 64 KiB, arena ou shadow, views UAV e buffer de faults, conforme memory.rs:1–48,230–239,680–708. Um acesso a página ausente nesse caminho pode ler zero ou perder uma escrita; o aviso de fault serve a frames posteriores. Isso é um limite de fidelidade documentado no código.

Para um mesmo intervalo pode haver backing de seção RAM com aliases de VA para os mesmos bytes, staging mapeado, espelho local DEFAULT, snapshots CPU, cópia de retorno/readback e, para um plano de textura, representação twin. Aliases de VA por si sós não multiplicam a RAM física. Buffers de hash/diretório acrescentam metadados e a alocação de heaps para texturas introduz outra granularidade. Não há máximo global inferível de “réplicas por asset” sem conhecer vistas, versões em voo e fences. O log 203447-7572:26578 mede working set de 21.640 MiB, private commit de 26.757 MiB e 3.855 MiB físicos disponíveis. Nenhum desses três valores mede VRAM ou tráfego PCIe.

**COMPROVADA no código, DESCONHECIDA para a ocorrência histórica.** Grafo esperado: leitura em lugar → MAPPING compartilhado (watch.rs:2945,3120) → acesso de CPU à view → VEH em watch.rs:3392–3416 → materialize_for, LAZY.read, depois PHYSICAL (xmem.rs:2572–2605). O remapeador adquire MAPPING exclusivo **antes de** PHYSICAL (xmem.rs:1235–1243). Isso documenta a ordem necessária ao leitor que sofre fault. O ciclo hipotético seria leitor segura MAPPING e aguarda PHYSICAL → escritor segura PHYSICAL e aguarda MAPPING → leitor. O callsite verificado do remapeador adota a ordem inversa à que fecharia esse ciclo; HOLDING também impede reentrada de materialize_for pela própria thread que detém PHYSICAL. Outro caminho de fault pode chamar demand::on_fault. A prova do incidente específico exige pilhas de todas as threads, endereço que falhou, dono de cada lock e contexto da cópia. Não confundir com wait_for_mapping da fila COPY auxiliar ou com a ausência no diretório de páginas de um shader.