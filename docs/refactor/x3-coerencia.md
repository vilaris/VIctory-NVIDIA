# X3 — Refreshes redundantes na thread de submissão

Na execução Series S com `VICTORY_INPLACE=1`, `copies` caiu de 45 ms e 72 MB por quadro para 18–24 ms e 42–48 MB. A espera pela GPU chegou a 23 ms, e a cadência ficou em 8–12 fps contra 13–14 fps na referência. O runtime abriu 77 janelas (364 MB) e recusou 642 em memória de views. Essa contagem mede janelas. Os MB por tipo de backing devem ser medidos com `rf-x3`.

A quebra de referência da thread de submissão era 73–81 ms por quadro. As etapas das listas custavam 31–36 ms, com 21–25 ms de refreshes e 7.840–10.270 pedidos já reconhecidos como limpos. A linha distingue `flushes` de `ranges`. O contador `known-clean` inclui pedidos descartados em `Batch::add` antes de um flush. Escritas sob flights consumiam 13–19 ms, o caminho de cópias da VRAM 13–17 ms e o write-watch inicial 4–6 ms.

## Instrumentação

Com `VICTORY_RF_X3_STATS=1`, uma linha `rf-x3:` reúne cerca de cinco segundos de presents e usa o mesmo denominador da linha de FPS. `submit-ms` e os quatro contadores do flush vêm da mesma coleta usada por `preparing per frame, the submitting thread`. `preparing` vem da linha de FPS. Em `checks`, cada valor é contagem por quadro seguida de ms por quadro.

| Campo | O que mede |
| --- | --- |
| `ahead-span`, `queue-span`, `merge-span` | Pedido coberto por um refresh adiantado, novo pedido na fila ou união com uma faixa pendente. O tempo destes checks fica em `steps`. |
| `memo-guard`, `memo-kept`, `memo-miss`, `early-ineligible` | Memo da cópia com publicação de guard ou cobertura da consulta mantida. Um miss exige comparar os stamps ou atualizar. `early-ineligible` inclui in-place, faixa vazia, memória sem tracking e shader write pendente. |
| `journal-guard-*`, `journal-kept-*`, `journal-unknown` | Comparação dos stamps de página/partes da cópia com o journal, com resultados `clean` e `dirty`. `journal-unknown` não tinha uma resposta vigente para a faixa. |
| `watch-ahead`, `watch-published`, `watch-kernel`, `watch-guards`, `watch-untracked` | Qual caminho `watch::writes` usou. O tempo inclui materialização, espera por locks e acesso ao journal quando esses passos ocorrem naquela chamada. |
| `update-clean`, `update-copied` | `Shadow::update` terminou sem bytes ou com bytes cobrados na linha de FPS. Inclui a consulta de `watch::writes` aninhada. |
| `front-wall`, `before-writes-wall`, `flush-wall`, `one-wall` | Tempos de parede do adiantamento inicial, dos refreshes adiantados antes de escritas, dos flushes comuns e do refresh individual antes de um shader write. Somam `submit-ms.refresh`, até arredondamento e fronteiras de coleta. |
| `fastskip`, `fast-miss`, `verify`, `verify-miss`, `reused-miss` | Prova rápida aceita, recusada, comparação com o caminho anterior, desacordo nessa comparação e releitura redundante do memo evitada no caminho ativo. |

`memo-*`, `journal-*`, `watch-*` e `update-*` são medidas de chamadas. Somar tempos de workers ou somar uma chamada ao seu subpasso dá um número maior que o tempo de parede. Use `*-wall` para fechar os 21–25 ms e as outras classes para localizar o custo. Os switches desligados deixam checagens de configuração em memória. Alocações, timers adicionais e agregação de eventos dependem dos switches.

### Objetos, bytes e backing

O top 20 ordena objetos pelo custo observado, com desempate por bytes e identidade estável `base_guest+tamanho/tipo/backing`. A contabilidade abrange toda a janela, inclusive a cauda fora do top 20. `copies.fps` e `textures.fps` são os totais de referência. `attributed` agrega as chamadas por objeto. `delta = fps − attributed` mostra a normalização de tempo dos lotes paralelos, planejamento coletivo e eventos cruzando a fronteira da amostra. `card_before`, `demand_fetch` e `descriptor-sync` aparecem em `other`. Os bytes de `card_before` são o payload dos dirty ranges, com o tempo de preparação da staging distribuído proporcionalmente quando `send::take` produz upload. `of-which-digest` conta bytes visitados pelo hash, dentro de `texture-refresh`. A quantidade de bytes de `refresh-watch` segue `Shadow::update`, inclusive caminhos que examinam uma região para descobrir os subtrechos a alterar. Fills em lote têm tempo rateado por bytes.

As colunas CBV/SRV/UAV/vertex/index/indirect contam bindings observáveis nas listas submetidas. `views` é o tamanho das views por binding, incluindo o limite da faixa do buffer quando conhecido. O tráfego físico depende de lanes, cache e índices. A instrumentação cobre tabelas diretas, entradas estaticamente conhecidas em tabelas reaproveitadas, vistas listadas e argumentos indiretos. Índices bindless calculados no shader, trabalho por lane e tabelas reaproveitadas que exigiriam reconstruir uma janela ou um alias ficam sem contagem completa. `shader_write=possible` cobre as declarações instrumentadas, UAVs de textura e sobreposição com objetos sinalizados. `unobserved` indica ausência de evidência na coleta. `rewrites` conta frames que voltaram a exigir refresh de bytes já vistos pelo write-watch, incluindo notificações de landing ou remap. `VICTORY_RF_X3_READ_LIMIT_MB`, 4 por padrão em MB decimais por quadro, filtra a triagem exploratória dos objetos. O resultado `NeedsSafetyProof` indica que os sinais de tráfego passaram na triagem.

`backing` mostra MB por quadro de `copies/textures` para cada classe. A classificação usa a extensão do objeto e a visão do mapeamento no momento da amostra. Uma extensão mista pode conter subtrechos individualmente abríveis. A consulta reaproveita metadados de `inplace` e não abre heaps.

| Classe | Método disponível ou impedimento |
| --- | --- |
| `address1-private` | Uma região privada, comprometida e RW cobre o objeto alinhado a 64 KiB. `OpenExistingHeapFromAddress1` é candidato técnico. |
| `filemap-sec-commit` | Uma sequência contígua de uma seção registrada `SEC_COMMIT`. `OpenExistingHeapFromFileMapping` é candidato técnico. |
| `address1-sec-reserve-open` | O port já abriu a região por `Address1` em uma view própria dos mesmos bytes. |
| `sec-reserve-filemap-refused` | O Windows recusa a seção `SEC_RESERVE` em `OpenExistingHeapFromFileMapping`. Um alias dedicado exigiria uma sequência materializada, comprometida e com vida útil comprovada. O page pool do xmem usa `SEC_RESERVE` no Windows nativo. |
| `soft`, `fragmented`, `unknown` | Views lógicas sem mapeamento host, lacunas ou trechos descontínuos, ou memória sem prova de um backing único admitido. |

O inventário inicial orienta onde procurar os 20 maiores objetos. A ordem e os endereços dos demais objetos dependem da cena e do conteúdo em streaming.

| Candidatos no top 20 | Origem no código | Tratamento |
| --- | --- | --- |
| Buffer de ~174 MB e outros buffers grandes de geometria, skinning e argumentos produzidos na GPU | `Shadow::update`, `Card::staging`, `Host::card_before` e `demand_fetch` | Mantêm as cópias. SRV/UAV extensos, grande volume de leituras e escritas do shader. |
| Janelas do alocador, especialmente views de buffers por endereço | `shadow::loose`, `Batch::refresh_front`, `Batch::flush_all` | Medir quantos pedidos terminam limpos e quantos exigem journal, kernel ou guards. |
| Buffers pequenos de vertex/index e argumentos de indiretos | `State::prepare_for`, `execute_indirect` | Cópias atuais ou caminho existente de `port::stream_bytes`; argumentos podem ter produtores GPU. |
| Constantes raiz, de tabelas e fora da região do port | `root_view`, `tables`, `HostList::defer_constants` | Aberturas atuais do port e fallback em buffer ou scratch. |
| Heaps de descritores e dados de tabelas | `sync_heaps`, `Step::Copy` | Traduz registros do título para descritores D3D12. Custos dependem dos registros tocados e da sincronização. |
| Texturas de streaming, mips materializados e planos | `Staging::refresh`, `Staging::digest`, `Step::Plane`, `defer_fill_buffer` | A leitura da origem, o digest e a disposição em staging têm funções distintas. Texturas swizzled exigem conversão. |

## FASTSKIP

`VICTORY_RF_X3_FASTSKIP=1` usa o primeiro stamp limpo do shadow como caminho rápido. A faixa coberta deve estar inteira em um bloco de 64 KiB protegido e publicado pelo watch, com geração atual igual à geração em que o legado confirmou a cópia. O teste rejeita aliases conhecidos, guards em abertura e escritas beside. A revisão e a assinatura do slot precisam continuar válidas. Somente shadows não carded, sem shader write conhecido e sem produtor pendente podem usar essa prova. O atalho de guard também exige `VICTORY_INPLACE=0`, pois um shader pode escrever por um binding in-place sem fault na CPU. Consultas ao kernel para memória privada continuam no caminho antigo.

O teste rápido é um subconjunto da prova do memo legado. Ele usa o mesmo contador dos blocos e o contador global, com duas leituras para detectar mudança durante a consulta dos bits. O write-watch avança gerações ao incorporar escritas ao journal. Uma falha de proteção revoga a publicação ao abrir o bloco. Writes do runtime e copy-back também usam notificações, revogação de guard ou bits beside em `watch.rs`; o remap limpa a publicação e invalida as gerações. As declarações ao shadow invalidam os stamps e estabelecem o estado pendente antes de escrever no host copy. Para DirectStorage que escreve por um alias de seção, mantém-se o contrato existente de visibilidade após a notificação de conclusão do xmem. Um evento sem informação de escritor observável não é motivo para promover uma faixa ao caminho rápido.

O modo ativo também pula a segunda leitura do mesmo memo quando o mesmo flush já falhou na primeira. Vai direto à comparação legada dos stamps com o journal. O flush mantém a união de spans da mesma cópia e as fronteiras entre escritas da lista. Um resultado limpo anterior, sozinho, não é licença para omitir checks pelo resto do quadro.

`VICTORY_RF_X3_VERIFY=1` junto de FASTSKIP executa toda a decisão legada no ponto original. `verify-miss` conta desacordos com o memo legado. Se a atualização real copiar bytes para o mesmo shadow e faixa, `rf-x3 verify diverge` traz endereço, tamanho e bytes. Também se verifica a geração na entrada de atualizações que o caminho legado ainda despachou. Comparações com writes concorrentes podem sinalizar diferença legítima. Um miss que passa por união de spans pode perder a correlação exata entre os dois eventos, por isso o contador de misses deve ser examinado junto dos avisos.

As etapas `putting the title's bytes under flights' writes` e `card_before` conservam sua ordem. Antes de paralelizá-las, seria preciso demonstrar que cada job tem ranges independentes e que a geração dos dados continua ligada à flight correta até o merge ou staging terminar. `send::take` já distribui o trabalho plano pelos helpers. A alteração não move os pontos de espera, de publicação ou de submissão na fila.

## In place existente

`VICTORY_INPLACE=0/1/2` continua configurando o mecanismo existente. O nível 2 é incompatível com `VICTORY_LATE_WAIT`, usado no jogo. A abertura de um heap a partir de endereço imobiliza as páginas (a sonda viu cerca de 70 ms para 256 MiB). Uma eventual extensão teria de reaproveitar a região aberta, usar o buffer placed com `ALLOW_CROSS_ADAPTER`, preservar a proteção, decommit e remap normais do título e manter os aliases até a fence. Tile mappings sobre esses heaps removem o device. O tempo de vida de cada slot de um ring precisa ser provado até a leitura da GPU, inclusive quando a CPU inicia o quadro seguinte. Um histórico de write-watch não dá essa garantia.

O registro do README de que o in place foi mais lento em 12 GB refere-se à versão que abria o buffer inteiro de 174 MB. O commit `4fdc16e` substituiu essa versão pelo port por acesso, mantendo a geometria e as escritas de skinning na cópia. O commit `340ece1` usa SRV raiz e views estruturadas de dwords para aceitar alinhamento de quatro bytes. O último resultado de FPS documentado para a revisão anterior foi 13,8 FPS em `play-20261127-114321-28084`; esse resultado ainda tinha 2.574–3.669 fills por quadro por causa do alinhamento de 256 bytes. O código posterior é a referência para o comportamento dos bindings, e o A/B Series S descrito acima é a referência de desempenho de abrir janelas. O experimento X3 preserva os bindings e não introduz um novo switch de zero-copy.

## Verificação no Windows

```powershell
cargo test -p victory-gpu --lib rf_x3::tests
```

O build e a execução ficam com o orquestrador. Para medir o runtime, use o mesmo executável e cena, o mesmo save e modo Series S/X, as mesmas configurações de frame generation e a mesma janela de amostragem depois de aquecer o cache. Grave as chaves abaixo em `%LOCALAPPDATA%\victory\victory-settings.txt` no formato `CHAVE=valor`. Reinicie o processo a cada variante. O launcher carrega esse arquivo.

1. Referência com `VICTORY_RF_X3_STATS=1`, `VICTORY_RF_X3_FASTSKIP=0` e `VICTORY_RF_X3_VERIFY=0`.
2. Com as estatísticas ligadas, configure `VICTORY_RF_X3_FASTSKIP=1` e `VICTORY_RF_X3_VERIFY=1`. Compare `fastskip`, `verify-miss` e os avisos `rf-x3 verify diverge`. Repita transições e streaming para exercitar remaps.
3. Com as estatísticas ligadas, deixe `VICTORY_RF_X3_FASTSKIP=1` e volte `VICTORY_RF_X3_VERIFY` para 0. Compare FPS, `preparing`, `copies` em ms e MB, `submit-ms.refresh`, `*-wall`, `known-clean` e `reused-miss`. As duas variantes de desempenho usam a mesma instrumentação. Faça também uma passagem com `STATS=0` se for medir o custo absoluto da instrumentação.

Ao comparar, pese as médias pela quantidade de frames em cada linha. Ganho no alvo deve aparecer no tempo de parede de refresh e nas etapas das listas. Queda de `copies` com divergência ou alteração visual é motivo para manter FASTSKIP desligado.
