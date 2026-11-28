# X9 · Coerência de memória e submissão

O branch parte de `399ebd4`. O X9 entrega um caminho opt-in para abertura de guardas por bitmap, refresh por intervalos, preparação paralela de uploads e comparação de descritores. O backend completo continua pendente. Em particular, a faixa de 174 MB continua compartilhada entre CPU e GPU, as views bindless ainda têm esperas durante a tradução e o lock de submissão continua serializando a preparação. O alvo de 33 ms permanece sem medição.

As referências de desempenho do coordenador são cerca de 45 ms de preparação olhando para uma parede (17,5 ms e 37 MB em cópias) e 93,5 ms dirigindo (50 ms e 85 MB em cópias, mais 19 ms e 259 MB em texturas). Os dois cenários exercitam custos diferentes. Até a fase de staging, as mudanças atuam nos dois eixos, usando os mesmos recursos e comandos da GPU.

## Fluxo implementado

```text
ExecuteCommandLists do título
  -> queue::behind se houver relay e cortes
  -> espera da fila do título resolvida, na ordem da fila
  -> Host::submit
       -> iniciar rodada de heaps em paralelo, quando assíncrona
       -> dependências de flights e consulta ao watch
       -> planejar intervalos e lançar refreshes elegíveis no pool
       -> executar passos CPU ordenados e montar listas
       -> join da última onda de buffers, texturas e fills
       -> snapshots e reserva de staging
       -> join do empacotamento de payload, pares e tiles
       -> join da rodada dos heaps e publicação de slots prontos
       -> ExecuteCommandLists + Signal
  -> relay aguarda fence host
       -> readback, merge, texback e fences observáveis pelo título
       -> aposentar a fatia de upload usada pela submissão
```

A fila mantém a ordenação por fila do título, incluindo cortes, esperas de outras filas e sinais. A captura usa a fila limitada existente. N+1 pode ser preparada enquanto N executa na GPU se as dependências de flights permitirem. O lock de submissão serializa a preparação. O patch reaproveita pools e filas existentes e não registra novas barreiras.

O pool recebe uma cópia ou grupo de cópias por job. Cada job processa os intervalos de uma cópia em série. A lista só consome a cobertura após o join. `Batch::title_written` termina o trabalho pendente e limpa a cobertura de buffers e texturas. Uma declaração GPU conserva o envelope do legado. A consulta antecipada ao watch e o teste de interseções com flights usam envelopes conservadores. A transferência pode usar a união das faixas explícitas em buffers host mapeados sem declarações ou flights de escrita e em submissões sem leitor bindless identificado.

```text
Copy / Placed / Run que podem escrever memória do título
  -> terminar as leituras pendentes -> efetuar a escrita -> abrir a onda seguinte

Depois do último passo CPU
  -> [refresh por shadow | detile por staging | fills por destino]
  -> join -> snapshots e ranges sujos
  -> [payload | tabelas de tiles | pares de palavras]
  -> join -> gravar exatamente os comandos de upload existentes

Resolveu flight ou passo CPU alterou heap depois do início da rodada
  -> rodada iniciada após a escrita -> join -> make_waiting -> Execute
```

O caminho de empacotamento reúne três fases CPU numa onda. A gravação dos comandos continua depois do join. A última onda de refresh mistura classes que só leem a memória do título e escrevem destinos separados. `fill_jobs` preserva a ordem dos fills quando os destinos se sobrepõem, inclusive em cadeias de sobreposição. Os steps que escrevem memória do título e as declarações GPU mantêm os respectivos pontos de ordenação. Em uma submissão que agrupa vários cortes, o planejamento antecipado de buffers mantém a cobertura conservadora do legado.

## Autoridade dos bytes

| Classe | Escritor e observação | Entrega dos bytes e transições |
| --- | --- | --- |
| CPU-stream | O título escreve os dados que `Fill` lê por extensão explícita. Para buffers privados com write-watch, o kernel indica as partes escritas de 4 KiB. | O scratch de constantes continua ligado à lista. Para envios de buffers carded, o X9 reserva uma fatia contígua e alinhada no ring de upload de 32 MiB e a mantém até `Host::card_done`. Uma fatia aposentada fora de ordem só libera espaço quando todas as fatias anteriores também estão aposentadas. Falta de espaço ou alocação usa o pool existente. Uma declaração GPU tira o buffer do planejamento estreito. |
| CPU-static | O título ou o runtime pode reescrever os dados. As views de seção usam guardas ou notificações de escrita. | Os bytes novos seguem nos uploads existentes antes dos leitores. Buffers carded conservam staging e VRAM. Um remap invalida as observações. Uma abertura por fault, por si só, não prova que um trecho pode ser promovido a CPU-stream sem guarda. |
| GPU-own, pendente | Uma declaração UAV pode cobrir uma view muito maior que as escritas reais. O buffer de aproximadamente 174 MB mistura constantes do título, geometria e saída de skinning. | A comparação e o merge existentes determinam o que a GPU escreveu. `demand` ainda pode estacionar alguns trechos na VRAM. Remover staging e comparações exige delimitar quais escritas chegam ao intervalo e garantir que acessos da CPU aguardem a versão correta. |
| Descritores | O título escreve por API, por bytes ou por passos CPU da submissão. O espelho consome páginas alteradas do cursor `Tracked`. A comparação agrupa registros por blocos e recoloca leituras incompletas na rodada seguinte. | A publicação passa por `mirror` e `make_waiting`, que resolvem a identidade e a prontidão da textura. Samplers com indexação direta dispensam a espera inicial de `note_indexed`, pois não têm uma passagem de descoberta de recursos antes da gravação. As views mantêm essas esperas onde necessárias. Uma rodada pendente pode atender várias solicitações que chegaram antes de seu início. |

**DirectStorage e remap.** As notificações do xmem propagam escritas por alias ao watch e invalidam descritores dependentes do mapeamento. Para aberturas por bitmap, um remap fecha a entrada atômica, espera as aberturas em curso e muda as views. A abertura fixa o mapeamento somente com a entrada aberta. Views aliased, materialização e conflitos de proteção usam o tratamento existente.

**Copy-back e texback.** As faixas de shader em voo ainda impedem refreshes que sobrescreveriam seus bytes. O relay mantém as fences, as comparações e o merge. O readback imediato ou sob demanda segue `demand`. O texback de água e física usa o caminho existente, invalidado nos remaps. Para remover a cópia CPU, a prova teria de incluir esse caminho e os acessos CPU fora de `safecopy`. O fetch de demanda atual pode recorrer à fila dedicada preexistente. A substituição precisa ordenar a leitura na fila host sem depender de uma thread parada no fault.

## Handler de escrita

```text
fault em bloco elegível de 64 KiB abaixo de 4 GiB
  -> fixar o mapeamento e bloquear mudança para PAGE_NOACCESS
  -> marcar OPENING / BESIDE, revogar PUBLISHED, avançar a geração
  -> VirtualProtect, limpar PROTECTED e marcar no bitmap
  -> devolver a escrita à thread do título

próximo detentor de GUARDS
  -> drenar o bitmap, reconciliar estados e liberar OPENING
  -> próximo consumo publica o resultado no journal
```

A abertura elegível evita `GUARDS`, a fila `OPENED` e alocação de metadados dentro da exceção. O bitmap é pré-alocado e marca a granularidade em que a proteção é removida. A abertura e o estacionamento usam operações ordenadas atomicamente para impedir que as duas rotas mudem a proteção ao mesmo tempo. O fallback ainda tem esperas e locks. Os faults tratados pelo bitmap entram na estatística própria de X9. O histórico de rearme de X5 observa os faults processados pelo seu caminho.

## Telemetria e verificação

- `VICTORY_RF_X9_STATS=1` emite `rf-x9:` na janela do medidor de FPS, em torno de cinco segundos. `submit-ms` é tempo de parede por quadro. `refresh` está dentro de `steps`. `jobs` soma chamadas que podem rodar no pool ou na thread chamadora. `upload-pack`, `texture` e `fill` detalham trabalho das ondas. Captura inclui backpressure. `record-heap-wait` mede chamadas que iniciam ou aguardam a rodada durante a gravação.
- Os bytes de refresh são agrupados por evidência de write-watch, guarda e memória sem tracking. `card-up` e `gpu-down` medem payload das operações da VRAM. `upload-ring` mede bytes reservados, incluindo o material de scatter e seu alinhamento. `texture-read` conta bytes de leitura na preparação de texturas. Refresh e upload da VRAM podem contabilizar os mesmos bytes em etapas diferentes. `gaps-excluded` mede lacunas excluídas do intervalo, inclusive bytes que o legado poderia reconhecer como limpos. `watch` contabiliza mudanças no mecanismo de observação do shadow. `parked-eager` conta novos blocos preferidos para landing imediato.
- `VICTORY_RF_X9_VERIFY=1` com X9 ativo compara prefixos de até 64 bytes de fills e de intervalos elegíveis com uma leitura pelo endereço do título. Também compara a origem de DMA de um envio direto por shadow com os bytes correspondentes do shadow legado. O verificador dos descritores usa um cursor legado independente e observa a chave publicada. `rf-x9 diverge` informa a faixa e a classe. A comparação após o scatter, dos bytes finais em VRAM, de GPU-own e de texback continua pendente.

## Limites de fidelidade e desempenho

- As declarações UAV de raiz e por indexação podem cobrir mais bytes que os shaders escrevem. Tratar a declaração inteira como propriedade exclusiva da GPU descartaria escritas válidas da CPU. O backend mantém a autoridade compartilhada, os hashes e o merge nessas faixas. Uma migração completa exige registrar a cobertura efetiva dos produtores, tomar posse antes do primeiro leitor da CPU e resolver remaps e aliases de DirectStorage enquanto a proteção é válida.
- As passagens `heap_buffers` e `early_textures` leem o espelho para descobrir trabalho de buffers e texturas a registrar. Tirar a espera mantendo essas passagens na tradução pode omitir o upload ou refresh do recurso que um shader bindless usa, inclusive nos bakes. A migração das views requer planejar a descoberta e produzir os uploads antes do consumidor na submissão. `make_waiting` sozinho faz a publicação dos slots prontos.
- O trabalho paralelo de CPU ocorre dentro da preparação de uma submissão, além do relay já existente. As versões de staging e de escrita vivem parcialmente nos próprios objetos. Preparar duas submissões CPU em paralelo requer versionar esses dados e transferir a dependência de publicação para a etapa ordenada de execução. O ring dá vida útil independente ao payload carded. Staging de texturas e snapshots conservam o modelo atual.
- O ring mantém 32 MiB adicionais de upload mapeado enquanto o processo está ativo. A primeira alocação pode afetar o aquecimento. As ondas mistas competem pelo pool e pela largura de banda da CPU. Os valores da telemetria e a semântica de publicação dos descritores precisam de A/B no jogo.

## Arquivos

| Arquivo em `crates/victory-gpu/src` | Alterações |
| --- | --- |
| `rf_x9.rs`, `lib.rs` | Switches, telemetria, checagem de amostras, `Ranges`, `upload_jobs`, `fill_jobs`, `UploadRing`, bitmap e sincronização atômica de remaps. |
| `watch.rs`, `demand.rs` | `open_x9`, reconciliação em `settle` e arbitragem de proteção com o estacionamento. |
| `host.rs`, `shadow.rs` | `Host::submit`, `Batch::copy_spans`, `Batch::flush_wave`, `FrontSpans`, `Shadow::take`, elegibilidade de refresh, verificação e medidas de cópias. |
| `host/send.rs`, `texture.rs` | `ring_memory`, `take_with`, `Own::record`, `give_back` e medição de `Staging::refresh`. |
| `descriptor.rs`, `rf_x4.rs` | `begin_sync_heaps`, `sync_heaps`, `note_indexed`, comparação em blocos e medição das esperas da gravação. |
| `queue.rs`, `queue/behind.rs` | Medição da captura e seleção da fila existente com os seus pré-requisitos. |

## Validação no Series S

Grave as chaves em `%LOCALAPPDATA%\victory\victory-settings.txt` e reinicie entre rodadas. Use o mesmo save, trecho, configuração de frame generation e switches X3/X4/X5. Descarte o aquecimento.

| Rodada | `VICTORY_RF_X9` | `VICTORY_RF_X9_VERIFY` | `VICTORY_RF_X9_STATS` |
| --- | --- | --- | --- |
| Referência instrumentada | 0 | 0 | 1 |
| Comparação de bytes | 1 | 1 | 1 |
| Medição | 1 | 0 | 1 |

Repita a cena olhando para uma parede e o trecho dirigindo. Compare cadência real, CPU de submissão, esperas de descritores, tempo de fault, uploads, readbacks e `rf-x9 diverge`. Exercite streaming, cortes de cena com remaps, geometria e skinning, bindless, bakes e água. O bitmap deve aparecer somente com X9 ativo e abertura elegível. Para retornar à referência, use `VICTORY_RF_X9=0` ou remova a chave.

Os testes novos cobrem união e cobertura de intervalos, retomada do envelope após declaração GPU, entrega pelo bitmap, exclusão durante remap, empacotamento de uploads, dependências por destino e aposentadoria fora de ordem no ring. Os testes existentes incluem `queue::behind::tests`, `heap_marks::tests`, `written_spans_tests` e os de `watch`, `demand` e `upload`. Compilação, execução dos testes e jogo ficam com o usuário. A validação desta entrega foi feita por inspeção e `git diff --check`.
