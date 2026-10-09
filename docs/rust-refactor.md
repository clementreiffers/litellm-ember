# Architecture Rust et validation

## Responsabilités

- `litellm/domain.rs` contient les agrégations, le calcul des périodes et les projections.
  L'instant, la date et le fuseau sont fournis explicitement. Les accumulateurs restent
  mutables et locaux ; les clés de regroupement empruntent les noms présents dans les logs.
- `litellm.rs` adapte les réponses HTTP, réutilise le transport Reqwest et mutualise
  les réponses identiques. Un verrou asynchrone par catégorie de réponse empêche
  deux téléchargements simultanés identiques. Les données conservées sont uniquement
  les champs utiles des logs, jamais les messages/prompt de la réponse LiteLLM.
- `service.rs` possède l'état publié et sérialise les commandes. Le réseau s'exécute
  dans des tâches indépendantes qui produisent des événements typés. Une sauvegarde
  annule le cycle et change la génération avant de toucher à la connexion.
- `settings.rs`, `persistence.rs` et `notify.rs` gèrent les effets persistants.
  Les opérations disque/Trousseau/notification passent par `spawn_blocking`.
  Le tray adapte les commandes Tauri et affiche les instantanés.

Les boucles et les mutations locales sont intentionnelles : le cœur fonctionnel
signifie ici « calculs déterministes sans effets externes », pas « aucune mutation ».

## Contrats et compatibilité

Les noms des commandes Tauri restent les mêmes. `Stats` ajoute `source_id`,
`generation`, `revision`, `cycle_id` et `refreshing`, tous avec valeurs par défaut
Serde. Les événements et `get_stats` utilisent le même instantané. L'interface
s'abonne avant de lire l'instantané et rejette les révisions plus anciennes.

`get_week` et `get_activity` renvoient `{ generation, data }`. Les deux côtés
rejettent les résultats d'une ancienne génération ; les appels d'onglets sont
annulés lorsqu'une nouvelle génération est publiée. Les identifiants locaux des
chargements empêchent également une réponse antérieure à une réinitialisation
locale d'écraser les données. Plusieurs invalidations pendant un appel produisent
une seule relance. Les listeners JS sont désinscrits à la destruction de leur owner.

Les champs de début et de reset conservent leur nom et transportent désormais un
instant ISO 8601 complet. Les anciennes dates `YYYY-MM-DD` sont toujours acceptées.
L'affichage reste une date. `s`, `m`, `h`, `d`, `w` respectent leur quantité exacte ;
`mo` conserve la convention de 30 jours. Une durée absente vaut sept jours ; une
durée invalide ne produit pas de projection. Les moyennes de coût et de tokens
incluent toutes les requêtes ; les percentiles de latence excluent les échecs.

Une identité de source aléatoire est enregistrée dans les préférences et les
caches. Elle ne contient ni la clé ni son empreinte. Les fichiers antérieurs sont
lisibles ; les caches sans identité vérifiable sont ignorés. Une configuration
existante valide sans identité reçoit un identifiant lors du chargement. Aucun
répertoire d'une autre application n'est importé.

## Durabilité et erreurs

La sauvegarde prépare et synchronise un fichier temporaire voisin, modifie le
Trousseau, puis remplace le JSON par renommage atomique. L'état en mémoire n'est
publié qu'après réussite. Un échec de renommage restaure la clé précédente ; un
échec de cette restauration bloque les requêtes jusqu'à une sauvegarde réussie.
Un emplacement de paramètres inaccessible ne produit jamais un faux succès.

Cela ne constitue pas une transaction atomique entre macOS Keychain et le système
de fichiers : un arrêt brutal entre les deux opérations reste une limite.
Les erreurs de lecture/suppression du Trousseau ne sont plus assimilées à une clé
absente. Les erreurs de transport, persistance et orchestration ont des types
internes ; la frontière IPC expose des messages français sans secrets.

Les notifications sont marquées après acceptation par le plugin, puis persistées
seulement si leur état change. Un refus d'envoi sera retenté au prochain cycle.
Un succès suivi d'un échec disque ne provoque pas de répétition dans la session,
mais peut être répété après redémarrage. L'acceptation par le plugin ne garantit
pas l'affichage par macOS (permissions et mode Concentration).

Un cycle publie progressivement le budget, les modèles et la journée, puis son
état terminé. Il écrit le cache une seule fois à la fin. Les cycles périodiques et
les rafraîchissements forcés repartent d'un cache réseau vide tout en conservant
le transport HTTP. Les onglets réutilisent les résultats disponibles durant
60 secondes ; un changement de journée ou de connexion invalide cette réutilisation.

## Mesures

Microbenchmark d'activité sur Apple Silicon, Rust 1.93.0, compilation optimisée,
20 modèles, 31 échantillons par taille. Désérialisation, réseau et disque exclus.
Comparaison alternée avec l'agrégation antérieure à la refonte, mêmes données.

| Lignes | Avant, médiane | Après, médiane |
| --- | ---: | ---: |
| 1 000 | 0,238 ms | 0,168 ms |
| 10 000 | 2,312 ms | 1,660 ms |
| 100 000 | 23,346 ms | 16,407 ms |

Le gain observé est d'environ 28–30 % sur ce jeu synthétique. Le seul retrait du
second tri avait un effet faible ; les gains viennent surtout du regroupement
sans allocation de nom par ligne et de la conversion de fuseau faite une seule
fois par ligne. Ce résultat n'est pas une mesure du temps d'affichage complet.

Reproduire la mesure de la version courante :

```sh
cargo bench -p ember --bench aggregations --locked
```

Les tests HTTP vérifient qu'une lecture simultanée Usage/Activité effectue un seul
appel détaillé, que Modèles/Semaine partagent un résumé identique, et qu'expiration
ou changement de journée provoquent un nouvel appel.

## Vérification

```sh
cargo fmt --all -- --check
cargo test -p ember -p shared -p ui --locked
cargo clippy -p ember -p shared --all-targets --all-features --locked -- -D warnings
cargo clippy -p ui --target wasm32-unknown-unknown --all-targets --all-features --locked -- -D warnings
npm test
NO_COLOR=true cargo tauri build --debug --no-bundle
```

Les régressions couvrent les réponses d'anciens cycles, les changements de source,
la coalescence du rafraîchissement, les échecs disque/Trousseau et la restauration,
les notifications refusées, les moyennes, les durées extrêmes, les journées locales,
l'expiration des caches et les transitions de chargement Leptos.

Le build natif macOS et le build WebAssembly de production ont été vérifiés.
La vérification visuelle du panneau et des notifications natives reste à effectuer :
aucun navigateur contrôlable ni contrôle d'application native n'était disponible
pendant cette session. Le test réseau réel reste volontairement ignoré.
