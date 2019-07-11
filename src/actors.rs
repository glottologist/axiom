///
/// Implements actors and the actor system.
use secc;
use secc::*;
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Formatter;
use std::hash::{Hash, Hasher};
use std::marker::{Send, Sync};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::thread::JoinHandle;
use uuid::Uuid;

/// This is a type used by the system for sending a message through a channel to the actor.
/// All messages are sent as this type and it is up to the message handler to cast the message
/// properly and deal with it. It is recommended that the user make use of the [`dispatch`]
/// utility function to help in the casting and calling operation.
pub type Message = dyn Any + Sync + Send;

/// Status of the message resulting from processing a message with an actor.
#[derive(Debug, Eq, PartialEq)]
pub enum Status {
    /// The message was processed and can be removed from the channel. Note that this doesn't
    /// necessarily mean that anything was done with the message, just that it can be removed.
    /// It is up to the mctor's essage processor to decide what if anything to do with the message.
    Processed,
    /// The message was skipped and should remain in the queue. Once a message is skipped then
    /// a skip tail will be created in the channel that will act as the actual tail until the
    /// [`ResetSkip'] status is returned from an actor's processor. This enables an actor to
    /// skip messages while working on a process and then clear the skip buffer and resume normal
    /// processing. This functionality is critical for actors that act as a finite state machine
    /// and thus might temporarily change the implementation of the processor and then switch
    /// back to a state where the previously sent messages are delivered.
    Skipped,
    /// Clears the skip tail on the channel. A skip tail is present when a message has been
    /// skipped by returning [`Skipped`] If no skip tail is set than this result is semantically
    /// the same as [`Processed`].
    ResetSkip,
}

/// An enum that holds a sender for an actor. This is usually wrapped in an actor id.
pub enum ActorSender {
    /// A Sender used for sending messages to local actors.
    Local(SeccSender<Arc<Message>>),
    /// The remote sender.
    Remote, // FIXME not implemented.
}

/// Encapsulates an ID to an actor.
pub struct ActorId {
    /// The unique id for this actor on this node.
    id: Uuid,
    /// The id for the node that this actor is on.
    node_id: Uuid,
    /// The handle to the sender side for the actor's message channel.
    sender: ActorSender,
    /// Holds a reference to the local actor system that the [ActorId] lives at.
    system: Arc<ActorSystem>,
}

impl fmt::Debug for ActorId {
    fn fmt(&self, formatter: &'_ mut Formatter) -> fmt::Result {
        write!(
            formatter,
            "ActorId{{ id: {}, node_id: {} }}",
            self.id.to_string(),
            self.node_id.to_string()
        )
    }
}

impl PartialEq for ActorId {
    fn eq(&self, other: &ActorId) -> bool {
        self.id == other.id && self.node_id == other.node_id
    }
}

impl Eq for ActorId {}

impl Hash for ActorId {
    fn hash<H: Hasher>(&self, state: &'_ mut H) {
        self.id.hash(state);
        self.node_id.hash(state);
    }
}

/// A type for a user function that processes messages for an actor. This will be passed to a
/// spawn funtion to specify the handler used for managing the state of the actor based
/// on the messages passed to the actor. The rewulting state passed will be used in the
/// next message call. The processor takes three arguments. First, the `state` is a mutable
/// reference to the current state of the actor. Second, is the [`ActorId`] enclosed in
/// an [`Arc`] to allow access to the actor system for spawning, sending to self and so on.
/// Third is the current message to process in an [`Arc`].
pub trait Processor<State: Send + Sync + 'static>:
    (FnMut(&mut State, Arc<ActorId>, &Arc<Message>) -> Status) + Send + Sync + 'static
{
}

// Allows any function, static or closure, to be used as a processor.
impl<F, State> Processor<State> for F
where
    State: Send + Sync + 'static,
    F: (FnMut(&mut State, Arc<ActorId>, &Arc<Message>) -> Status) + Send + Sync + 'static,
{
}

/// An actual actor in the system. An actor is a worker that allows the asynchronous
/// processing of messages. The actor will obey a certain set of rules, namely.
/// 1. An actor can be interracted with only by means of messages.
/// 2. An actor processes ONLY one message at a time.
/// 3. An actor will process a message only once.
/// 4. An actor can send a message to any other actor without knowledge of that actor's internals
/// 5. Actors send only immutable data as messages, though they may have mutable internal state.
/// 6. Actors are location agnostic. An actor can be sent a message from anywhere in the cluster.
pub struct Actor {
    /// Id of the associated actor.
    aid: Arc<ActorId>,
    /// Receiver for the actor channel.
    receiver: SeccReceiver<Arc<Message>>,
    /// Function processes messages sent to the actor wrapped in a closure to erase the
    /// state type that the actor is maanaging.
    handler: Mutex<Box<dyn (FnMut(Arc<ActorId>, &Arc<Message>) -> Status) + Send + Sync + 'static>>,
}

impl Actor {
    /// Creates a new actor context with the given actor id. The user will pass the initial state
    /// of the actor as well as the processor that will be used to process messages sent to the
    /// actor. The system and node id are passed separately because of restrictions on mutex
    /// guards not being re-entrant in rust.
    pub fn new<'a, F, State>(
        system: Arc<ActorSystem>,
        node_id: Uuid,
        mut state: State,
        mut processor: F,
    ) -> Arc<Actor>
    where
        State: Send + Sync + 'static,
        F: Processor<State>,
    {
        // TODO Let the user pass the size of the channel queue when creating the actor.
        // Create the channel for the actor.
        let (tx, receiver) = secc::create::<Arc<Message>>(32);
        // The sender will be put inside the actor id.
        let mut aid = ActorId {
            id: Uuid::new_v4(),
            node_id: node_id,
            sender: ActorSender::Local(tx),
            system: system.clone(),
        };

        // This handler will manage the state for the actor.
        let handler = Box::new({
            move |aid: Arc<ActorId>, message: &Arc<Message>| processor(&mut state, aid, message)
        });

        // The actor context will be the holder of the actual actor.
        let actor = Actor {
            aid: Arc::new(aid),
            receiver,
            handler,
        };

        Arc::new(actor)
    }

    /// fetches the actor id for this actor.
    fn aid(&self) -> Arc<ActorId> {
        self.aid.clone()
    }

    /// Returns the total number of messages that have been sent to this actor.
    fn sent(&self) -> usize {
        self.receiver.sent()
    }

    /// Returns the total number of messages that have been received by this actor.
    fn received(&self) -> usize {
        self.receiver.received()
    }

    /// Returns the total number of messages that are currently pending in the actor's channel.
    fn pending(&self) -> usize {
        self.receiver.pending()
    }

    /// Receive as message off the channel and processes it with the actor.
    fn receive(&mut self) {
        match self.receiver.peek() {
            Result::Err(_err) => {
                // TODO Log the error from the channel.
                return;
            }
            Result::Ok(message) => {
                let result: &Arc<Message> = message;
                let result = (*self.handler)(self.aid.clone(), message);
                let queue_result = match result {
                    Status::Processed => self.receiver.pop(),
                    Status::Skipped => self.receiver.skip(),
                    Status::ResetSkip => self.receiver.reset_skip(),
                };
                match queue_result {
                    Ok(_) => (),
                    Err(e) => println!("Error Occurred {:?}", e), // TODO Log this
                };
            }
        }
    }
}

/// An actor system that contains and manages the actors spawned inside it. The actor system
/// embodies principally two main components, a AID table that maps [ActorId]s to the actors
/// and a run channel that the actors are sent to when they have pending messages.
pub struct ActorSystem {
    /// Id for this actor system and node.
    pub node_id: Uuid,
    /// Sender side of the work channel. When an actor gets a message and its pending count goes
    /// from 0 to 1 it will put itself in the channel via the sender.
    sender: Arc<SeccSender<Arc<ActorId>>>,
    /// Receiver side of the work channel. All threads in the pool will be grabbing actor ids
    /// from this receiver. When a thread gets an actor it will lock the mutex of the actor
    /// inside the id and then will process the message. If the actor has pending messages after
    /// the processing is done then the actor will be put back in the channel to be re-queued.
    receiver: Arc<SeccReceiver<Arc<ActorId>>>,
    /// Holds the [Actor] objects keyed by the [ActorId]. The [RwLock] will be locked for write
    /// only when a new actor is spawned but otherwise will be locked for read by the threads
    /// in the threadbool as they try to look up actors to process.
    actors_by_aid: Arc<RwLock<HashMap<Arc<ActorId>, Arc<Actor>>>>,
    /// Holds handles to the pool of threads processing the run queue.
    thread_pool: Vec<JoinHandle<()>>,
}

impl ActorSystem {
    /// Creates an actor system with the given size for the dispatcher channel and thread pool.
    /// The user should benchmark how many slots in the run channel and the number of threads
    /// they need in order to satisfy the requirements of the system they are creating.
    pub fn create(run_channel_size: u16, thread_pool_size: u16) -> Arc<ActorSystem> {
        // We will use arcs for the sender and receivers because the they will surf threads.
        let (sender, receiver) = secc::create_with_arcs::<Arc<ActorId>>(run_channel_size);

        // The map is created with a mutex for interior mutability.
        let actors_by_aid = Arc::new(RwLock::new(HashMap::new()));

        // Allocate the thread pool of threads grabbing for work in the channel.
        let thread_pool: Vec<JoinHandle<()>> = (1..thread_pool_size)
            .map(|_i| {
                ActorSystem::start_dispatcher_thread(
                    actors_by_aid,
                    sender.clone(),
                    receiver.clone(),
                )
            })
            .collect();

        Arc::new(ActorSystem {
            node_id: Uuid::new_v4(),
            sender,
            receiver,
            actors_by_aid,
            thread_pool,
        })
    }

    /// Starts a thread for the dispatcher that will process actor messages.
    fn start_dispatcher_thread(
        actors_by_aid: Arc<RwLock<HashMap<Arc<ActorId>, Arc<Actor>>>>,
        sender: Arc<SeccSender<Arc<ActorId>>>,
        receiver: Arc<SeccReceiver<Arc<ActorId>>>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            match receiver.receive_await() {
                Err(_) => println!("Error"), // FIXME turn this into logging instead.
                Ok(aid) => {
                    // FIXME Actor panic shouldnt take down the whole code
                    let guard = actors_by_aid.read().unwrap();
                    let actor = guard.get(aid).unwrap();
                    (*actor).receive();
                    if actor.receiver.receivable() > 0 {
                        // if there are additional messages pending in the actor,
                        // re-enqueue the message at the back of the queue.
                        sender.send_await(aid.clone());
                    }
                }
            }
        })
    }

    /// Schedules the `actor_id` for work on the given actor system.
    fn schedule(aid: Arc<ActorId>) {
        // Note that this is implemented here rather than calling the sender directly
        // from the send in order to allow internal optimization of the actor system.
        aid.system.sender.send(aid);
    }
}

/// Spawns a new actor on the actor `system` using the given `starting_state` for the actor and
/// the given `processor` function that will be used to process actor messages.
pub fn spawn<'a, F, State>(system: Arc<ActorSystem>, state: State, processor: F) -> Arc<ActorId>
where
    State: Send + Sync + 'static,
    F: Processor<State>,
{
    let mut guard = system.actors_by_aid.write().unwrap();
    let actor = Actor::new(system.clone(), system.node_id, state, processor);
    guard.insert(actor.aid.clone(), actor);
    actor.aid()
}

/// Sends the `message` to the actor identified by the `aid` passed.
pub fn send(aid: Arc<ActorId>, message: Arc<Message>) {
    match &aid.sender {
        ActorSender::Local(sender) => {
            let readable = sender.send_await(message).unwrap();
            // From 0 to 1 message readable triggers schedule.
            if readable == 1 {
                ActorSystem::schedule(aid.clone())
            }
        }
        _ => unimplemented!("Remote actors not implemented currently."),
    }
}

/// Attempts to downcast the Arc<Message> to the specific arc type of the handler and then call
/// that handler with the message. If the dispatch is successful it will return the result of the
/// call to the caller, otherwise it will return None back to the caller.
pub fn dispatch<T: 'static, S, R>(
    aid: Arc<ActorId>,
    state: &mut S,
    message: Arc<Message>,
    mut func: impl FnMut(Arc<ActorId>, &mut S, &T) -> R,
) -> Option<R> {
    match message.downcast_ref::<T>() {
        Some(x) => Some(func(aid.clone(), state, x)),
        None => None,
    }
}

// --------------------- Test Cases ---------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    enum Operation {
        Inc,
        Dec,
    }

    struct Data {
        count: i32,
        value: i32,
    }

    /*
     *    fn handle_op(aid: arc<actorid>, data: &mut data, msg: operation) -> status {
     *        match msg {
     *            operation::inc => data.count += 1,
     *            operation::dec => data.count -= 1,
     *        }
     *        status::processed
     *    }
     *
     *    fn handle_i32(aid: arc<actorid>, data: &mut data, msg: i32) -> status {
     *        data.value += msg;
     *        status::processed
     *    }
     */

    /*
     *fn handle(aid: Arc<ActorId>, data: &mut Data, msg: Arc<Message>) -> Status {
     *    dispatch(aid, data, msg.clone(), handle_op)
     *        .or_else(|| dispatch(aid, data, msg.clone(), handle_i32))
     *        .unwrap()
     *}
     */

    #[test]
    fn test_spawn_with_closure() {
        let system = ActorSystem::create(10, 1);
        let data = Data { count: 0, value: 0 };
        let aid = spawn(system, data, |state, aid, msg| {
            println!("Got a Message");
            Status::Processed
        });
        send(aid, Arc::new(Operation::Inc));
        send(aid, Arc::new(Operation::Inc));
        send(aid, Arc::new(10 as i32));

        // TODO Assert states after messages.
    }
}
