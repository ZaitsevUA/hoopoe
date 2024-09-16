

/* ========================================================================================
       REALTIME NOTIF EVENT STREAMING DESIGN PATTERN (README files inside tests folder)
 
    local & remote RPC channels : Kafka, RMQ, Redis pubsub, jobq mpsc, pg db 
    thread safe eventloop queue : the receiver of each broker or the mpsc channel like Arc<Mutex<Recevier<Data>>>
    queue                       : buffer of thread safe event objects like Buffer<Event>{data:Arc<Mutex<Vec<Event>>>}
    syntax                      : while let Some(notif_data) = mpscReceiverEventloopOrRmqOrKafkaOrRedisSubscriber.recv().await{}
    CronSchedulerActor          : an actor background worker to call io periodically using ctx.run_interval(), loop{} and tokio time and spawn or redis key space notifications pubsub
    storing and caching         : store and cache received notif data in pg db and on redis
    node talking                : local talking with mailbox & remote talking with (JSON/G/Capnp)RPC
    websocket config            : send received notif data through the ws mpsc sender / receive notif data in ws server scope 
    notify notif owner          : through http short polling api or websocket streaming
    async io runTime Executor   : using tokio spawn to spawn light io thread per each async io task  
    NotifBrokerActor use cases  :
        → wallet service prodcons actor with different payment portal => receive command object
        → transactionpool service prodcons actor                      => receive transaction object
        → product service prodcons actor                              => receive order object to purchase atomically
    concurrency tools & notes   : 
        → an eventloop is a thread safe receiver queue of the mpsc channel which receives tasks and execute them in free background thread
        → tokio::select, tokio::spawn, tokio::sync::{Mutex, mpsc, RwLock}, std::sync::{Condvar, Arc, Mutex}
        → cpu tasks are graph and geo calculations as well as cryptography algorithms which are resource intensive
        → async io tasks are io and networking calls which must be handled simultaneously in order to scale resources
        → async io task execution inside light threadpool: wait on the task but don't block the thread, continue with executing other tasks
        → cpu task execution inside os threadpool: suspend the thread of execution by blocking it, avoid executing other tasks 
        → use await on the async io task to not to block the thread and let the thread execute other tasks meanwhile the task is waiting to be solved
        → await pauses and suspends the function execution not the thread and tells the eventloop to pop out another task while the function is awaited
        → use join on the cpu task to block the thread to suspend the thread execution with all tasks and wait for the result of the thread
        → use Condvar to wait and block the thread until some data changes then notify the blocked thread
        → don't use os threadpool in the context of light threadpool, they block the execution thread as well as the entire async runtime
        → std mutex block the caller thread if the lock is busy it stops the thread from executing all tasks until it acquires the lock
        → tokio mutex suspend the async task if the lock is busy it suspend the io task instead of blocking the executor thread
        → std stuffs block and suspend the thread and stop it from executing other tasks while it doing some heavy operations inside the thread like mutex logics
        → tokio stuffs suspend the async io task process instead of blocking the thread and allows the thread executing other tasks simultaneously
        → use channels for atomic syncing between threads instead of using mutex in both async and none async context
        → if we want some result of an either async io or cpu task we have the options of either using of mutex, channels or joining on the thread (would block cpu threads) 
        → as soon as the future or async io task is ready to yeild a value the runtime meanwhile of handling other tasks would notify the caller about the result
        → as soon as the the result of the task is ready to be returned from the os thread the os thread will be stopped blocking and continue with executing other tasks
        → actors have their own os or ligh thread of execution which uses to spawn tasks they've received via message passing channels or mailbox
        → to share a data between threads it must be Send Sync and live valid
        → initialize storage and actors data structures once and pack them in AppContext struct then share this between threads

    ========================================================================================
        a sexy actor to produce/consume messages from different type of brokers
        it uses RMQ, Redis and Kafka to produce and consume massive messages in 
        realtime, kindly it supports data AES256 encryption through producing 
        messages to the broker. we can send either producing or consuming message 
        to this actor to start producing or consuming in the background 

        brokering is all about queueing, sending and receiving messages way more faster, 
        safer and reliable than a simple eventloop or a tcp based channel. 
        all brokers contains message/task/job queue to handle communication between services
        asyncly, they've been designed specially for decoupling tight long running services 
        due to their durability nature like giving predicting output of an ai model service
        while the ai model is working on other input prediction in the background we can 
        receive the old outputs and pass them through the brokers to receive them in some 
        http service for responding the caller.   
        In rmq producer sends message to exchange the a consumer can bind its queue to 
        the exchange to receive the messages, routing key determines the pattern of receive 
        messages inside the bounded queue from the exchange 
        In kafka producer sends messages to topic the consumer can receives data from 
        the topic, Rmq adds an extra layer on top of msg handling logic which is creating 
        queue per each consumer.
        Offset in kafka is an strategy which determines the way of tracking the sequential 
        order of receiving messages by kafka topics it’s like routing key in rmq 

    BROKER TYPES: 
        ---> KAFKA
        ---> REDIS PUBSUB
        ---> RMQ 
            -ˋˏ✄┈┈┈┈
            >_ the consuming task has been started by sending the ConsumeNotif message 
            to this actor which will execute the streaming loop over the queue in 
            either the notif consumer actor context itself or the tokio spawn thread:

                notif consumer -----Q(Consume Payload)-----> Exchange -----notif CQRS writer-----> cache/store on Redis & db

            -ˋˏ✄┈┈┈┈
            >_ the producing task has been started by sending the ProduceNotif message 
            to this actor which will execute the publishing process to the exchange 
            in either the notif producer actor context itself or the tokio spawn thread:

                notif producer -----payload-----> Exchange
            
            -ˋˏ✄┈┈┈┈
            >_ client can use a short polling technique to fetch notifications for an 
            specific owner from redis or db, this is the best solution to implement a
            realtiming strategy on the client side to fetch what's happening on the 
            server side in realtime. there is another expensive alternaitive to this
            which is startiung a websocket server in backend side and send all notifications
            to the client through the ws channels in realtime.

             _________                                       _________
            | server1 | <------- RMQ notif broker -------> | server2 |
             ---------                                       ---------
                | ws                                          | ws
                 ------------------- client ------------------
    ======================================================================================== */

use tokio::spawn;
use crate::*;
use deadpool_lapin::lapin::protocol::channel;
use deadpool_redis::redis::AsyncCommands;
use base58::FromBase58;
use constants::CRYPTER_THEMIS_ERROR_CODE;
use constants::FILE_ERROR_CODE;
use deadpool_redis::redis::RedisResult;
use futures::executor;
use models::event::NotifData;
use actix::prelude::*;
use actix::Handler as ActixMessageHandler;
use actix::{Actor, AsyncContext, Context, Addr};
use config::EnvExt;
use constants::{MAILBOX_CHANNEL_ERROR_CODE, PING_INTERVAL};
use deadpool_lapin::lapin::options::{BasicAckOptions, BasicConsumeOptions, ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions};
use deadpool_lapin::lapin::protocol::exchange;
use deadpool_lapin::lapin::types::FieldTable;
use deadpool_lapin::lapin::BasicProperties;
use interfaces::*;
use storage::engine::Storage;
use wallexerr::misc::SecureCellConfig;
use wallexerr::misc::Wallet;
use workers::cqrs::mutators::notif::NotifMutatorActor;
use workers::cqrs::mutators::notif::StoreNotifEvent;
use workers::zerlog::ZerLogProducerActor;
use std::error::Error;
use std::io::Read;
use std::sync::Arc;
use crate::models::event::*;
use crate::interfaces::crypter::Crypter;


#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct PublishNotifToKafka{
    pub topic: String,
    pub local_spawn: bool,
    pub notif_data: NotifData, 
    pub encryptionConfig: Option<CryptoConfig>
}

#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct ConsumeNotifFromKafka{ // we must build a unique consumer per each consuming process 
    pub topic: String,
    pub consumerId: String,
}

#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct PublishNotifToRedis{
    pub channel: String,
    pub local_spawn: bool,
    pub notif_data: NotifData, 
    pub encryptionConfig: Option<CryptoConfig>
}

#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct ConsumeNotifFromRedis{
    pub channel: String,
}

#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct HealthMsg{
    pub shutdown: bool
}

#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct ProduceNotif{
    pub local_spawn: bool, // either spawn in actor context or tokio threadpool
    pub notif_data: NotifData,
    pub exchange_name: String,
    pub exchange_type: String,
    pub routing_key: String,
    pub encryption_config: Option<CryptoConfig>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
pub struct CryptoConfig{
    pub secret: String,
    pub passphrase: String,
    pub unique_redis_id: String,
}

#[derive(Message, Clone, Serialize, Deserialize, Debug, Default, ToSchema)]
#[rtype(result = "()")]
pub struct ConsumeNotif{ // we'll create a channel then start consuming by binding a queue to the exchange
    /* -ˋˏ✄┈┈┈┈ 
        following queue gets bounded to the passed in exchange type with its 
        routing key, when producer wants to produce notif data it sends them 
        to the exchange with a known routing key, any queue that is bounded 
        to that exchange routing key will be filled up with messages coming 
        from the producer and they stay in there until a consumer read them
    */
    pub queue: String,
    pub exchange_name: String,
    /* -ˋˏ✄┈┈┈┈ 
        routing_key is pattern for the exchange to route the messages to the 
        bounded queue.
        multiple producers can send their messages to a single exchange but 
        each of with different routing keys.
        any queue that is bounded to the exchange routing key will receive 
        all the messages that follows the pattern inside the routing_key.
        a message can be sent from producer to an exchange in a topic way with 
        an sepecific routing key which tells the exchange this is the way of 
        receiving messages that a bounded queue does since we might have 
        sent messages to the same exchange with multiple different routing 
        keys per each message and for a queue that is bounded to the exchange 
        with the passed in routing key can only receives the messages that 
        follow the pattern in the selected routing key. so the routing key in 
        consumer is the patterns for this queue to tell exchange to what 
        messages this queue is interested in:

        1) producer produces messages and send them to the exchange with an specific routing key
        2) a consumer create its own queue and bind it to the exchange with the bind key that 
           is interested to receive the message from the exchange based on that key.
                                                                                                                 --------          ---------
                                                                                                                | queue1 | <----- |consumer1|
                                                                        ------> routing_key1 <---------------------------          ---------
                                                                       |                                            
        producer1 ----------                                       -----------------> routing_key0               
                            |____ messages > routing_key1 ------> | exchange|                                                
                             ____ messages > routing_key4 ------>  -----------------> routing_key2                                     
                            |                                          |                                --------        -----------
       producer2 -----------                                           |                               | queue2 | <----| consumer2 |
                                                                        ------> routing_key4 <------------------        -----------

    */
    pub routing_key: String, // patterns for this queue to tell exchange what messages this queue is interested in
    pub tag: String,
    pub redis_cache_exp: u64,
    pub local_spawn: bool, // either spawn in actor context or tokio threadpool
    pub store_in_db: bool,
    pub decryption_config: Option<CryptoConfig>
}


#[derive(Clone)]
pub struct NotifBrokerActor{
    pub notif_broker_sender: tokio::sync::mpsc::Sender<String>, // use to send notif data to mpsc channel for ws
    pub app_storage: std::option::Option<Arc<Storage>>, // REQUIRED: communicating with third party storage
    pub notif_mutator_actor: Addr<NotifMutatorActor>, // REQUIRED: communicating with mutator actor to write into redis and db 
    pub zerlog_producer_actor: Addr<ZerLogProducerActor> // REQUIRED: send any error log to the zerlog queue
} 

impl Actor for NotifBrokerActor{
    
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {

        log::info!("🎬 NotifBrokerActor has started");    

        ctx.run_interval(PING_INTERVAL, |actor, ctx|{
            
            let this = actor.clone();
            let addr = ctx.address();

            tokio::spawn(async move{

                // check something constantly, schedule to be executed 
                // at a certain time in the background lightweight thread
                // ...
                
            });

        });

    }
}

impl NotifBrokerActor{

    /* ******************************************************************************* */
    /* ***************************** PUBLISHING TO REDIS ***************************** */
    /* ******************************************************************************* */
    pub async fn publishToRedis(&self, channel: &str, notif_data: NotifData, encryptionConfig: Option<CryptoConfig>){
        
        let storage = self.app_storage.clone();
        let redis_pool = storage.as_ref().unwrap().get_redis_pool().await.unwrap();
        let zerlog_producer_actor = self.clone().zerlog_producer_actor;


        // publish easily to redis
        // ...
    

    }
    
    /* ******************************************************************************** */
    /* ***************************** CONSUMING FROM REDIS ***************************** */
    /* ******************************************************************************** */
    pub async fn consumeFromRedis(&self, channel: &str){

        // start consuming from redis
        // ...

    }
    
    /* ********************************************************************* */
    /* ***************************** CONSUMING ***************************** */
    /* ********************************************************************* */
    // if a consumer service wants to read notifs received from the rmq it 
    // needs to either fetch from redis or db the method doesn't return 
    // anything back to the caller
    pub async fn consume(&self, exp_seconds: u64,
        consumer_tag: &str, queue: &str, 
        binding_key: &str, exchange: &str,
        store_in_db: bool,
        decryption_config: Option<CryptoConfig>
    ){

        let storage = self.app_storage.clone();
        let rmq_pool = storage.as_ref().unwrap().get_lapin_pool().await.unwrap();
        let redis_pool = storage.unwrap().get_redis_pool().await.unwrap();
        let notif_mutator_actor = self.notif_mutator_actor.clone();
        let zerlog_producer_actor = self.clone().zerlog_producer_actor;
        let notif_data_sender = self.notif_broker_sender.clone();
        
        match rmq_pool.get().await{
            Ok(conn) => {

                let create_channel = conn.create_channel().await;
                match create_channel{
                    Ok(chan) => {

                        let mut q_options = QueueDeclareOptions::default();
                        q_options.durable = true; // durability is the ability to restore data on node shutdown

                        // -ˋˏ✄┈┈┈┈ making a queue inside the broker per each consumer, 
                        let create_queue = chan
                            .queue_declare(
                                &queue,
                                q_options,
                                FieldTable::default(),
                            )
                            .await;

                        let Ok(q) = create_queue else{
                            let e = create_queue.unwrap_err();
                            use crate::error::{ErrorKind, HoopoeErrorResponse};
                            let error_content = &e.to_string();
                            let error_content = error_content.as_bytes().to_vec();
                            let mut error_instance = HoopoeErrorResponse::new(
                                *constants::STORAGE_IO_ERROR_CODE, // error code
                                error_content, // error content
                                ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                "NotifBrokerActor.queue_declare", // method
                                Some(&zerlog_producer_actor)
                            ).await;
                            return; // terminate the caller
                        };

                        // binding the queue to the exchange routing key to receive messages it interested in
                        /* -ˋˏ✄┈┈┈┈ 
                            if the exchange is not direct or is fanout or topic we should bind the 
                            queue to the exchange to consume the messages from the queue. binding 
                            the queue to the passed in exchange, if the exchange is direct every 
                            queue that is created is automatically bounded to it with a routing key 
                            which is the same as the queue name, the direct exchange is "" and 
                            rmq doesn't allow to bind any queue to that manually
                        */
                        match chan
                            .queue_bind(q.name().as_str(), &exchange, &binding_key, 
                                QueueBindOptions::default(), FieldTable::default()
                            )
                            .await
                            { // trying to bind the queue to the exchange
                                Ok(_) => {},
                                Err(e) => {
                                    use crate::error::{ErrorKind, HoopoeErrorResponse};
                                    let error_content = &e.to_string();
                                    let error_content = error_content.as_bytes().to_vec();
                                    let mut error_instance = HoopoeErrorResponse::new(
                                        *constants::STORAGE_IO_ERROR_CODE, // error code
                                        error_content, // error content
                                        ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                        "NotifBrokerActor.queue_bind", // method
                                        Some(&zerlog_producer_actor)
                                    ).await;
                                    return; // terminate the caller
                                }
                            }

                        // since &str is not lived long enough to be passed to the tokio spawn
                        // if it was static it would be good however we're converting them to
                        // String to pass the String version of them to the tokio spawn scope
                        let cloned_consumer_tag = consumer_tag.to_string();
                        let cloned_queue = queue.to_string();
                        let cloned_notif_data_sender_channel = notif_data_sender.clone();

                        /* =================================================================================== */
                        /* ================================ CONSUMING PROCESS ================================ */
                        /* =================================================================================== */
                        // start consuming in the background in a lightweight thread of execution
                        // receiving is considered to be none blocking which won't block the current thread. 
                        tokio::spawn(async move{

                            // try to find the secure cell config on the redis and 
                            // do passphrase and secret key validation logic before
                            // consuming messages
                            let mut secure_cell_config = if let Some(mut config) = decryption_config.clone(){
                                match redis_pool.get().await{
                                    Ok(mut redis_conn) => {
                    
                                        // get the secure cell config from redis cache
                                        let redis_key = format!("notif_encryption_config_for_{}", config.unique_redis_id);
                                        let is_key_there: bool = redis_conn.exists(&redis_key).await.unwrap();
                                        
                                        let secure_cell_config = if is_key_there{
                                            let get_secure_cell: String = redis_conn.get(redis_key).await.unwrap();
                                            serde_json::from_str::<SecureCellConfig>(&get_secure_cell).unwrap()
                                        } else{
                                            SecureCellConfig::default()
                                        };
    
                                        config.secret = hex::encode(config.secret);
                                        config.passphrase = hex::encode(config.passphrase);

                                        // make sure that both passphrase and secret key are the same 
                                        // inside the stored secure cell config on redis
                                        if config.passphrase != secure_cell_config.passphrase || 
                                            config.secret != secure_cell_config.secret_key{
                                                log::error!("[!] wrong passphrase or secret key");
                                                return; // terminate the caller and cancel consuming, api must gets called again
                                            }
                                        
                                        // return the loaded instance from redis
                                        secure_cell_config
    
                                    },
                                    Err(e) => {
                                        use crate::error::{ErrorKind, HoopoeErrorResponse};
                                        let error_content = &e.to_string();
                                        let error_content_ = error_content.as_bytes().to_vec();
                                        let mut error_instance = HoopoeErrorResponse::new(
                                            *constants::STORAGE_IO_ERROR_CODE, // error code
                                            error_content_, // error content
                                            ErrorKind::Storage(crate::error::StorageError::RedisPool(e)), // error kind
                                            "NotifBrokerActor.produce.redis_pool", // method
                                            Some(&zerlog_producer_actor)
                                        ).await;
    
                                        SecureCellConfig::default()
                                    }
                                }

                            } else{
                                SecureCellConfig::default()
                            };

                            // -ˋˏ✄┈┈┈┈ consuming from the queue owned by this consumer
                            match chan
                                .basic_consume(
                                    /* -ˋˏ✄┈┈┈┈
                                        the queue that is bounded to the exchange to receive messages based on the routing key
                                        since the queue is already bounded to the exchange and its routing key, it only receives 
                                        messages from the exchange that matches and follows the passed in routing pattern like:
                                        message routing key "orders.processed" might match a binding with routing key "orders.#
                                        if none the messages follow the pattern then the queue will receive no message from the 
                                        exchange based on that pattern! although messages are inside the exchange.
                                    */
                                    &cloned_queue, 
                                    &cloned_consumer_tag, // custom consumer name
                                    BasicConsumeOptions::default(), 
                                    FieldTable::default()
                                )
                                .await
                            {
                                Ok(mut consumer) => {
                                    // stream over consumer to receive data from the queue
                                    while let Some(delivery) = consumer.next().await{
                                        match delivery{
                                            Ok(delv) => {

                                                log::info!("[*] received delivery from queue#<{}>", q.name());

                                                // if the consumer receives this data from the queue
                                                match delv.ack(BasicAckOptions::default()).await{
                                                    Ok(ok) => {

                                                        let consumed_buffer = delv.data;
                                                        
                                                        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
                                                        // ===>>>===>>>===>>>===>>>===>>> data decryption logic ===>>>===>>>===>>>===>>>===>>>
                                                        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
                                                        // if we have a config means the data has been encrypted
                                                        let data = if !secure_cell_config.clone().data.is_empty(){

                                                            // if the decryption config is available then consumed_buffer is the hex of encrypted data
                                                            let hexed_data = std::str::from_utf8(&consumed_buffer).unwrap();
                                                            let encrypted_data_buffer = hex::decode(hexed_data).unwrap();

                                                            // // make sure that the encrypted data on redis is the same as the one consumed 
                                                            // // by the consumer, we should first convert the base58 back to the original buffer
                                                            if secure_cell_config.data.clone() != encrypted_data_buffer || 
                                                                secure_cell_config.data.len() != encrypted_data_buffer.len(){

                                                                    log::error!("received encrypted data != redis encrypted data, CHANNEL IS NOT SAFE!");
                                                                    return; // terminate the caller, cancel the rest of computations
                                                                }

                                                            /*  ====-----====-----====-----====-----====-----
                                                                easier way for decryption using Crypter trait
                                                                ====-----====-----====-----====-----====-----
                                                            */ 
                                                            let encrypted_data_buffer = secure_cell_config.data.clone(); // data is the encrypted buffer
                                                            let stringified_encrypted_data_buffer = serde_json::to_string_pretty(&encrypted_data_buffer).unwrap();
                                                            stringified_encrypted_data_buffer.decrypt(&mut secure_cell_config);
                                                            let raw_data = secure_cell_config.data.clone();
                                                            let raw_data_str = std::str::from_utf8(&raw_data).unwrap().to_string();

                                                            // pass the secure_cell_config instance to decrypt the data, note 
                                                            // that the instance must contains the encrypted data in form of utf8 bytes
                                                            match Wallet::secure_cell_decrypt(&mut secure_cell_config){ // passing the redis secure_cell_config instance
                                                                Ok(data) => {
                                                                    // data is the raw utf8 bytes of the actual data
                                                                    std::str::from_utf8(&data).unwrap().to_string()
                                                                },
                                                                Err(e) => {

                                                                    let zerlog_producer_actor = zerlog_producer_actor.clone(); // clone the old one in each iteration
                                                                    let source = &e.to_string(); // we know every goddamn type implements Error trait, we've used it here which allows use to call the source method on the object
                                                                    let err_instance = crate::error::HoopoeErrorResponse::new(
                                                                        *CRYPTER_THEMIS_ERROR_CODE, // error hex (u16) code
                                                                        source.as_bytes().to_vec(), // text of error source in form of utf8 bytes
                                                                        crate::error::ErrorKind::Crypter(crate::error::CrypterError::Themis(e)), // the actual source of the error caused at runtime
                                                                        &String::from("NotifBrokerActor.consume.Wallet::secure_cell_decrypt"), // current method name
                                                                        Some(&zerlog_producer_actor)
                                                                    ).await;
                                                                      
                                                                    // can't decrypt return the raw base58 string of encrypted data
                                                                    // this can't be decoded to NotifData we'll get serde error log!
                                                                    std::str::from_utf8(&consumed_buffer).unwrap().to_string()
                                                                }
                                                            }

                                                        } else{
                                                            // no decryption config is needed, just return the raw data
                                                            // there would be no isse with decoding this into NotifData
                                                            log::error!("encrypted data is empty in secure_cell_config instance");
                                                            std::str::from_utf8(&consumed_buffer).unwrap().to_string()
                                                        };
                                                        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
                                                        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
                                                        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>

                                                        // either decrypted or the raw data as string
                                                        log::info!("[*] received data: {}", data);
                                                        
                                                        let get_notif_event = serde_json::from_str::<NotifData>(&data);
                                                        match get_notif_event{
                                                            Ok(notif_event) => {

                                                                log::info!("[*] deserialized data: {:?}", notif_event);

                                                                // =================================================================
                                                                /* -------------------------- send to mpsc channel for ws streaming
                                                                // =================================================================
                                                                    this is the most important part in here, we're slightly sending the data
                                                                    to downside of a jobq mpsc channel, the receiver however will receive data in 
                                                                    websocket handler which enables us to send realtime data received from RMQ 
                                                                    to the client through websocket server: RMQ over websocket
                                                                    once we receive the data from the mpsc channel in websocket handler we'll 
                                                                    send it to the client through websocket channel.
                                                                */
                                                                if let Err(e) = cloned_notif_data_sender_channel.send(data).await{
                                                                    log::error!("can't send notif data to websocket channel due to: {}", e.to_string());
                                                                }

                                                                // =============================================================================
                                                                // ------------- if the cache on redis flag was activated we then store on redis
                                                                // =============================================================================
                                                                if exp_seconds != 0{
                                                                    match redis_pool.get().await{
                                                                        Ok(mut redis_conn) => {

                                                                            // key: String::from(notif_receiver.id) | value: Vec<NotifData>
                                                                            let redis_notif_key = format!("notif_owner:{}", &notif_event.receiver_info);
                                                                            
                                                                            // -ˋˏ✄┈┈┈┈ extend notifs
                                                                            let get_events: RedisResult<String> = redis_conn.get(&redis_notif_key).await;
                                                                            let events = match get_events{
                                                                                Ok(events_string) => {
                                                                                    let get_messages = serde_json::from_str::<Vec<NotifData>>(&events_string);
                                                                                    match get_messages{
                                                                                        Ok(mut messages) => {
                                                                                            messages.push(notif_event.clone());
                                                                                            messages
                                                                                        },
                                                                                        Err(e) => {
                                                                                            use crate::error::{ErrorKind, HoopoeErrorResponse};
                                                                                            let error_content = &e.to_string();
                                                                                            let error_content_ = error_content.as_bytes().to_vec();
                                                                                            let mut error_instance = HoopoeErrorResponse::new(
                                                                                                *constants::CODEC_ERROR_CODE, // error code
                                                                                                error_content_, // error content
                                                                                                ErrorKind::Codec(crate::error::CodecError::Serde(e)), // error kind
                                                                                                "NotifBrokerActor.decode_serde_redis", // method
                                                                                                Some(&zerlog_producer_actor)
                                                                                            ).await;
                                                                                            return; // terminate the caller
                                                                                        }
                                                                                    }
                                                                    
                                                                                },
                                                                                Err(e) => {
                                                                                    // we can't get the key means this is the first time we're creating the key
                                                                                    // or the key is expired already, we'll create a new key either way and put
                                                                                    // the init message in there.
                                                                                    let init_message = vec![
                                                                                        notif_event.clone()
                                                                                    ];

                                                                                    init_message

                                                                                }
                                                                            };

                                                                            // -ˋˏ✄┈┈┈┈ caching the notif event in redis with expirable key
                                                                            // chaching in redis is an async task which will be executed 
                                                                            // in the background with an expirable key
                                                                            tokio::spawn(async move{
                                                                                let events_string = serde_json::to_string(&events).unwrap();
                                                                                let is_key_there: bool = redis_conn.exists(&redis_notif_key.clone()).await.unwrap();
                                                                                if is_key_there{ // update only the value
                                                                                    let _: () = redis_conn.set(&redis_notif_key.clone(), &events_string).await.unwrap();
                                                                                } else{ // initializing a new expirable key containing the new notif data
                                                                                    /*
                                                                                        make sure you won't get the following error:
                                                                                        called `Result::unwrap()` on an `Err` value: MISCONF: Redis is configured to 
                                                                                        save RDB snapshots, but it's currently unable to persist to disk. Commands that
                                                                                        may modify the data set are disabled, because this instance is configured to 
                                                                                        report errors during writes if RDB snapshotting fails (stop-writes-on-bgsave-error option). 
                                                                                        Please check the Redis logs for details about the RDB error. 
                                                                                        SOLUTION: restart redis :)
                                                                                    */
                                                                                    let _: () = redis_conn.set_ex(&redis_notif_key.clone(), &events_string, exp_seconds).await.unwrap();
                                                                                }
                                                                            });

                                                                        },
                                                                        Err(e) => {
                                                                            use crate::error::{ErrorKind, HoopoeErrorResponse};
                                                                            let error_content = &e.to_string();
                                                                            let error_content_ = error_content.as_bytes().to_vec();
                                                                            let mut error_instance = HoopoeErrorResponse::new(
                                                                                *constants::STORAGE_IO_ERROR_CODE, // error code
                                                                                error_content_, // error content
                                                                                ErrorKind::Storage(crate::error::StorageError::RedisPool(e)), // error kind
                                                                                "NotifBrokerActor.redis_pool", // method
                                                                                Some(&zerlog_producer_actor)
                                                                        ).await;
                                                                            return; // terminate the caller
                                                                        }
                                                                    }
                                                                }

                                                                // =============================================================================
                                                                // ------------- if the store in db flag was activated we then store in database
                                                                // =============================================================================
                                                                if store_in_db{
                                                                    // -ˋˏ✄┈┈┈┈ store notif in db by sending message to the notif mutator actor worker
                                                                    // sending StoreNotifEvent message to the notif event mutator actor
                                                                    // spawning the async task of storing data in db in the background
                                                                    // worker of lightweight thread of execution using tokio threadpool
                                                                    tokio::spawn(
                                                                        {
                                                                            let cloned_message = notif_event.clone();
                                                                            let cloned_mutator_actor = notif_mutator_actor.clone();
                                                                            let zerlog_producer_actor = zerlog_producer_actor.clone();
                                                                            async move{
                                                                                match cloned_mutator_actor
                                                                                    .send(StoreNotifEvent{
                                                                                        message: cloned_message,
                                                                                        local_spawn: true
                                                                                    })
                                                                                    .await
                                                                                    {
                                                                                        Ok(_) => { () },
                                                                                        Err(e) => {
                                                                                            let source = &e.to_string(); // we know every goddamn type implements Error trait, we've used it here which allows use to call the source method on the object
                                                                                            let err_instance = crate::error::HoopoeErrorResponse::new(
                                                                                                *MAILBOX_CHANNEL_ERROR_CODE, // error hex (u16) code
                                                                                                source.as_bytes().to_vec(), // text of error source in form of utf8 bytes
                                                                                                crate::error::ErrorKind::Actor(crate::error::ActixMailBoxError::Mailbox(e)), // the actual source of the error caused at runtime
                                                                                                &String::from("NotifBrokerActor.notif_mutator_actor.send"), // current method name
                                                                                                Some(&zerlog_producer_actor)
                                                                                            ).await;
                                                                                            return;
                                                                                        }
                                                                                    }
                                                                            }
                                                                        }
                                                                    );
                                                                }

                                                            },
                                                            Err(e) => {
                                                                log::error!("[!] can't deserialized into NotifData struct");
                                                                use crate::error::{ErrorKind, HoopoeErrorResponse};
                                                                let error_content = &e.to_string();
                                                                let error_content_ = error_content.as_bytes().to_vec();
                                                                let mut error_instance = HoopoeErrorResponse::new(
                                                                    *constants::CODEC_ERROR_CODE, // error code
                                                                    error_content_, // error content
                                                                    ErrorKind::Codec(crate::error::CodecError::Serde(e)), // error kind
                                                                    "NotifBrokerActor.decode_serde", // method
                                                                    Some(&zerlog_producer_actor)
                                                                ).await;
                                                                return; // terminate the caller
                                                            }
                                                        }
                                                    },
                                                    Err(e) => {
                                                        use crate::error::{ErrorKind, HoopoeErrorResponse};
                                                        let error_content = &e.to_string();
                                                        let error_content = error_content.as_bytes().to_vec();
                                                        let mut error_instance = HoopoeErrorResponse::new(
                                                            *constants::STORAGE_IO_ERROR_CODE, // error code
                                                            error_content, // error content
                                                            ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                                            "NotifBrokerActor.consume_ack", // method
                                                            Some(&zerlog_producer_actor)
                                                        ).await;
                                                        return; // terminate the caller
                                                    }
                                                }
                    
                                            },
                                            Err(e) => {
                                                use crate::error::{ErrorKind, HoopoeErrorResponse};
                                                let error_content = &e.to_string();
                                                let error_content = error_content.as_bytes().to_vec();
                                                let mut error_instance = HoopoeErrorResponse::new(
                                                    *constants::STORAGE_IO_ERROR_CODE, // error code
                                                    error_content, // error content
                                                    ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                                    "NotifBrokerActor.consume_getting_delivery", // method
                                                    Some(&zerlog_producer_actor)
                                                ).await;
                                                return; // terminate the caller 
                                            }
                                        }
                                    }
                                },
                                Err(e) => {
                                    use crate::error::{ErrorKind, HoopoeErrorResponse};
                                    let error_content = &e.to_string();
                                    let error_content = error_content.as_bytes().to_vec();
                                    let mut error_instance = HoopoeErrorResponse::new(
                                        *constants::STORAGE_IO_ERROR_CODE, // error code
                                        error_content, // error content
                                        ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                        "NotifBrokerActor.consume_basic_consume", // method
                                        Some(&zerlog_producer_actor)
                                    ).await;
                                    return; // terminate the caller 
                                }
                            }

                        });

                    },
                    Err(e) => {
                        use crate::error::{ErrorKind, HoopoeErrorResponse};
                        let error_content = &e.to_string();
                        let error_content = error_content.as_bytes().to_vec();
                        let mut error_instance = HoopoeErrorResponse::new(
                            *constants::STORAGE_IO_ERROR_CODE, // error code
                            error_content, // error content
                            ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                            "NotifBrokerActor.consume_create_channel", // method
                            Some(&zerlog_producer_actor)
                        ).await;
                        return; // terminate the caller   
                    }
                }

            },
            Err(e) => {
                use crate::error::{ErrorKind, HoopoeErrorResponse};
                let error_content = &e.to_string();
                let error_content = error_content.as_bytes().to_vec();
                let mut error_instance = HoopoeErrorResponse::new(
                    *constants::STORAGE_IO_ERROR_CODE, // error code
                    error_content, // error content
                    ErrorKind::Storage(crate::error::StorageError::RmqPool(e)), // error kind
                    "NotifBrokerActor.consume_pool", // method
                    Some(&zerlog_producer_actor)
                ).await;
                return; // terminate the caller
            }
        };

    }

    /* ********************************************************************* */
    /* ***************************** PRODUCING ***************************** */
    /* ********************************************************************* */
    pub async fn produce(&self, data: &str, exchange: &str, routing_key: &str, exchange_type: &str, unique_redis_id: &str, secure_cell_config: &mut SecureCellConfig){

        let zerlog_producer_actor = self.clone().zerlog_producer_actor;
        let this = self.clone();

        // these are must be converted into String first to make longer lifetime 
        // cause &str can't get moved into tokio spawn as its lifetime it's not 
        // static the tokio spawn lives longer than the &str and the &str gets 
        // dropped out of the ram once the function is finished with executing
        let exchange = exchange.to_string();
        let routing_key = routing_key.to_string();
        let exchange_type = exchange_type.to_string();
        let data = data.to_string();
        let cloned_secure_cell_config = secure_cell_config.clone();
        let unique_redis_id = unique_redis_id.to_string();

        tokio::spawn(async move{

            let storage = this.clone().app_storage.clone();
            let rmq_pool = storage.as_ref().unwrap().get_lapin_pool().await.unwrap();
            let redis_pool = storage.as_ref().unwrap().get_redis_pool().await.unwrap();


            // make sure that we have redis unique id and encrypted data in secure cell
            // then cache the condif on redis with expirable key
            if !unique_redis_id.is_empty() && !cloned_secure_cell_config.data.is_empty(){
                match redis_pool.get().await{
                    Ok(mut redis_conn) => {
                        
                        log::info!("[*] caching secure cell config on redis");

                        // cache the secure cell config on redis for 5 mins
                        // this is faster than storing it on disk or file
                        let str_secure_cell = serde_json::to_string_pretty(&cloned_secure_cell_config).unwrap();
                        let redis_key = format!("notif_encryption_config_for_{}", unique_redis_id);
                        let _: () = redis_conn.set_ex(redis_key, str_secure_cell, 300).await.unwrap();
                    },
                    Err(e) => {
                        use crate::error::{ErrorKind, HoopoeErrorResponse};
                        let error_content = &e.to_string();
                        let error_content_ = error_content.as_bytes().to_vec();
                        let mut error_instance = HoopoeErrorResponse::new(
                            *constants::STORAGE_IO_ERROR_CODE, // error code
                            error_content_, // error content
                            ErrorKind::Storage(crate::error::StorageError::RedisPool(e)), // error kind
                            "NotifBrokerActor.produce.redis_pool", // method
                            Some(&zerlog_producer_actor)
                        ).await;
                    }
                };
            }
            
            // trying to ge a connection from the pool
            match rmq_pool.get().await{
                Ok(pool) => {

                    // -ˋˏ✄┈┈┈┈ creating a channel in this thread
                    match pool.create_channel().await{
                        Ok(chan) => {

                            let mut ex_options = ExchangeDeclareOptions::default();
                            ex_options.auto_delete = true; // the exchange can only be deleted automatically if all bindings are deleted
                            ex_options.durable = true; // durability is the ability to restore data on node shutdown

                            // -ˋˏ✄┈┈┈┈ creating exchange
                            /* 
                                you should set the auto_delete flag to True when declaring the exchange. This will 
                                automatically delete the exchange when all channels are done with it.
                                Keep in mind that this means that it will stay as long as there is an active binding 
                                to the exchange. If you delete the binding, or queue, the exchange will be deleted.
                                if you need to keep the queue, but not the exchange you can remove the binding once 
                                you are done publishing. This should automatically remove the exchange.
                                so when all bindings (queues and exchanges) get deleted the exchange will be deleted.
                            */
                            match chan
                                .exchange_declare(&exchange, {
                                    match exchange_type.as_str(){
                                        "fanout" => deadpool_lapin::lapin::ExchangeKind::Fanout,
                                        "direct" => deadpool_lapin::lapin::ExchangeKind::Direct,
                                        "headers" => deadpool_lapin::lapin::ExchangeKind::Headers,
                                        _ => deadpool_lapin::lapin::ExchangeKind::Topic,
                                    }
                                },
                                ex_options, FieldTable::default()
                                )
                                .await
                                {
                                    Ok(ok) => {ok},
                                    Err(e) => {
                                        use crate::error::{ErrorKind, HoopoeErrorResponse};
                                        let e_string = &e.to_string();
                                        let error_content = e_string.as_bytes().to_vec();
                                        let mut error_instance = HoopoeErrorResponse::new(
                                            *constants::STORAGE_IO_ERROR_CODE, // error code
                                            error_content, // error content
                                            ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                            "NotifBrokerActor.exchange_declare", // method
                                            Some(&zerlog_producer_actor)
                                        ).await;
                                        
                                    }

                                };

                            /* =================================================================================== */
                            /* ================================ PRODUCING PROCESS ================================ */
                            /* =================================================================================== */
                            // async task: publish messages to exchange in the background in a lightweight thread of execution
                            tokio::spawn(async move{
                                
                                // -ˋˏ✄┈┈┈┈ publishing to exchange from this channel,
                                // later consumer bind its queue to this exchange and its
                                // routing key so messages go inside its queue, later they 
                                // can be consumed from the queue by the consumer
                                use deadpool_lapin::lapin::options::BasicPublishOptions;
                                let payload = data.as_bytes();
                                match chan
                                    .basic_publish(
                                        &exchange, // messages go in there
                                        &routing_key, // the way that message gets routed to the queue based on a unique routing key
                                        BasicPublishOptions::default(),
                                        payload, // this is the ProduceNotif data,
                                        BasicProperties::default().with_content_type("application/json".into()),
                                    )
                                    .await
                                    {
                                        Ok(pc) => {
                                            let get_confirmation = pc.await;
                                            let Ok(confirmation) = get_confirmation else{
                                                use crate::error::{ErrorKind, HoopoeErrorResponse};
                                                let error_content_ = get_confirmation.unwrap_err();
                                                let e_string = &error_content_.to_string();
                                                let error_content = e_string.as_bytes().to_vec();
                                                let mut error_instance = HoopoeErrorResponse::new(
                                                    *constants::STORAGE_IO_ERROR_CODE, // error code
                                                    error_content, // error content
                                                    ErrorKind::Storage(crate::error::StorageError::Rmq(error_content_)), // error kind
                                                    "NotifBrokerActor.get_confirmation", // method
                                                    Some(&zerlog_producer_actor)
                                                ).await;

                                                return; // needs to terminate the caller in let else pattern
                                            };

                                        },
                                        Err(e) => {
                                            use crate::error::{ErrorKind, HoopoeErrorResponse};
                                            let error_content = &e.to_string();
                                            let error_content = error_content.as_bytes().to_vec();
                                            let mut error_instance = HoopoeErrorResponse::new(
                                                *constants::STORAGE_IO_ERROR_CODE, // error code
                                                error_content, // error content
                                                ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                                "NotifBrokerActor.basic_publish", // method
                                                Some(&zerlog_producer_actor)
                                            ).await;

                                        }
                                    }
                            });

                        },
                        Err(e) => {
                            use crate::error::{ErrorKind, HoopoeErrorResponse};
                            let error_content = &e.to_string();
                            let error_content = error_content.as_bytes().to_vec();
                            let mut error_instance = HoopoeErrorResponse::new(
                                *constants::STORAGE_IO_ERROR_CODE, // error code
                                error_content, // error content
                                ErrorKind::Storage(crate::error::StorageError::Rmq(e)), // error kind
                                "NotifBrokerActor.create_channel", // method
                                Some(&zerlog_producer_actor)
                            ).await;

                        }
                    }
                },
                Err(e) => {

                    use crate::error::{ErrorKind, HoopoeErrorResponse};
                    let error_content = &e.to_string();
                    let error_content = error_content.as_bytes().to_vec();
                    let mut error_instance = HoopoeErrorResponse::new(
                        *constants::STORAGE_IO_ERROR_CODE, // error code
                        error_content, // error content
                        ErrorKind::Storage(crate::error::StorageError::RmqPool(e)), // error kind
                        "NotifBrokerActor.produce_pool", // method
                        Some(&zerlog_producer_actor)
                    ).await;

                }
            };
            
        });
        
    
    }

    pub fn new(app_storage: std::option::Option<Arc<Storage>>, 
        notif_mutator_actor: Addr<NotifMutatorActor>,
        zerlog_producer_actor: Addr<ZerLogProducerActor>,
        notif_broker_sender: tokio::sync::mpsc::Sender<String>) -> Self{
        Self { app_storage, notif_mutator_actor, zerlog_producer_actor, notif_broker_sender }
    }

}

/* ********************************************************************************* */
/* ***************************** PRODUCE NOTIF HANDLER ***************************** */
/* ********************************************************************************* */
impl ActixMessageHandler<ProduceNotif> for NotifBrokerActor{
    
    type Result = ();
    fn handle(&mut self, msg: ProduceNotif, ctx: &mut Self::Context) -> Self::Result {

        // unpacking the notif data
        let ProduceNotif { 
                exchange_name,
                exchange_type,
                routing_key,
                local_spawn,
                notif_data,
                encryption_config,

            } = msg.clone();
        
        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
        // ===>>>===>>>===>>>===>>>===>>> data encryption logic ===>>>===>>>===>>>===>>>===>>>
        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
        let stringified_config_data = if let Some(config) = encryption_config{

            // hex encoding the secret and passphrase
            let mut secure_cell_config = &mut wallexerr::misc::SecureCellConfig{
                secret_key: hex::encode(config.secret),
                passphrase: hex::encode(config.passphrase),
                data: serde_json::to_vec(&notif_data).unwrap(),
            };

            /*  ====-----====-----====-----====-----====-----
                easier way for encryption using Crypter trait
                ====-----====-----====-----====-----====-----
            */
            let notif_data_str = serde_json::to_string_pretty(&notif_data).unwrap();
            notif_data_str.encrypt(secure_cell_config);
            let encrypted_data = secure_cell_config.data.clone(); // it now contains the encrypted data
            let encrypted_data_hex_string = hex::encode(&encrypted_data);
            

            let str_data = match Wallet::secure_cell_encrypt(secure_cell_config){
                Ok(data) => {
                    use base58::ToBase58;
                    let stringified_data = hex::encode(&data);

                    // the data in secure_cell_config must be the encrypted data for future decryption
                    // we need to update the data field right after the encryption since we're storing 
                    // the secure_cell_config instance on redis which must contains the encrypted data.
                    secure_cell_config.data = data; 

                    stringified_data // this can also be a base58 or base64
                },
                Err(e) => {
                    let zerlog_producer_actor = self.zerlog_producer_actor.clone();
                    // log the error in the a lightweight thread of execution inside tokio threads
                    // since we don't need output or any result from the task inside the thread thus
                    // there is no channel to send data to outside of tokio::spawn, the writing however
                    // consists of file and rmq (network) operations which are none blocking async io
                    // processes need to get executed in a light thread instead of cpu thread.
                    tokio::spawn(async move{
                        let source = &e.to_string(); // we know every goddamn type implements Error trait, we've used it here which allows use to call the source method on the object
                        let err_instance = crate::error::HoopoeErrorResponse::new(
                            *CRYPTER_THEMIS_ERROR_CODE, // error hex (u16) code
                            source.as_bytes().to_vec(), // text of error source in form of utf8 bytes
                            crate::error::ErrorKind::Crypter(crate::error::CrypterError::Themis(e)), // the actual source of the error caused at runtime
                            &String::from("NotifBrokerActor.ActixMessageHandler<ProduceNotif>.Wallet::secure_cell_encrypt"), // current method name
                            Some(&zerlog_producer_actor)
                        ).await;
                    });
                    // return the actual data with no encryption
                    serde_json::to_string_pretty(&notif_data).unwrap()
                }
            };

            (str_data, secure_cell_config.to_owned(), config.unique_redis_id)

        } else{
            // we can't return a reference to a SecureCellConfig in here cause 
            // it's a variable which has defined in the current scope and once 
            // this scope gets executed and dropped the instance hence any pointer 
            // of it won't be valid. so don't return a mutable pointer to SecureCellConfig
            // instance in here.
            (serde_json::to_string_pretty(&notif_data).unwrap(), SecureCellConfig::default(), String::from(""))
        };
        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>
        // ===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>>===>>

        let this = self.clone();

        let (
            stringified_data, 
            mut secure_cell_config, 
            unique_redis_id
        ) = stringified_config_data;

        // spawn the future in the background into the given actor context thread
        // by doing this we're executing the future inside the actor thread since
        // every actor has its own thread of execution.
        if local_spawn{
            async move{
                this.produce(&stringified_data, &exchange_name, &routing_key, &exchange_type, &unique_redis_id, &mut secure_cell_config).await;
            }
            .into_actor(self) // convert the future into an actor future of type NotifBrokerActor
            .spawn(ctx); // spawn the future object into this actor context thread
        } else{ // spawn the future in the background into the tokio lightweight thread
            tokio::spawn(async move{
                this.produce(&stringified_data, &exchange_name, &routing_key, &exchange_type, &unique_redis_id, &mut secure_cell_config).await;
            });
        }
        
        return;
        
    }

}

/* ********************************************************************************* */
/* ***************************** CONSUME NOTIF HANDLER ***************************** */
/* ********************************************************************************* */
impl ActixMessageHandler<ConsumeNotif> for NotifBrokerActor{
    
    type Result = ();
    fn handle(&mut self, msg: ConsumeNotif, ctx: &mut Self::Context) -> Self::Result {

        // unpacking the consume data
        let ConsumeNotif { 
                queue, 
                tag,
                exchange_name,
                routing_key,
                redis_cache_exp,
                local_spawn,
                store_in_db,
                decryption_config

            } = msg.clone(); // the unpacking pattern is always matched so if let ... is useless
        
        let this = self.clone();
        
        // spawn the future in the background into the given actor context thread
        // by doing this we're executing the future inside the actor thread since
        // every actor has its own thread of execution.
        if local_spawn{
            async move{
                this.consume(
                    redis_cache_exp, 
                    &tag, 
                    &queue, 
                    &routing_key, 
                    &exchange_name,
                    store_in_db,
                    decryption_config
                ).await;
            }
            .into_actor(self) // convert the future into an actor future of type NotifBrokerActor
            .spawn(ctx); // spawn the future object into this actor context thread
        } else{ // spawn the future in the background into the tokio lightweight thread
            tokio::spawn(async move{
                this.consume(
                    redis_cache_exp, 
                    &tag, 
                    &queue, 
                    &routing_key, 
                    &exchange_name,
                    store_in_db,
                    decryption_config
                ).await;
            });
        }
        return; // terminate the caller

    }

}

impl ActixMessageHandler<HealthMsg> for NotifBrokerActor{
    type Result = ();
    fn handle(&mut self, msg: HealthMsg, ctx: &mut Self::Context) -> Self::Result {
        if msg.shutdown{
            ctx.stop(); // stop the already running actor
        }
    }
}

/* **************************************************************************************** */
/* ***************************** REDIS PRODUCER NOTIF HANDLER ***************************** */
/* **************************************************************************************** */
impl ActixMessageHandler<PublishNotifToRedis> for NotifBrokerActor{
   
    type Result = ();
    
    fn handle(&mut self, msg: PublishNotifToRedis, ctx: &mut Self::Context) -> Self::Result { 

        
        let PublishNotifToRedis{ 
            channel, 
            local_spawn,
            notif_data, 
            encryptionConfig 
        } = msg.clone();

        let this = self.clone();
        let task = async move{
            
            // await on the publishing task, tells runtime we need the result 
            // of this task, if it's ready runtime return the result back to 
            // the caller otherwise suspend the this.publishToRedis() function
            // until the task is ready to be polled, meanwhile it executes other
            // tasks (won't block the thread)
            this.publishToRedis(&channel, notif_data, encryptionConfig).await;

        };  

        
        // spawn the task in the background ligh thread or 
        // the actor thread itself.
        // don't await on the spawn; let the task execute 
        // in the background unless you want to use select
        // or tell the runtime someone needs the result right
        // now but notify later once the task is completed 
        // and don't block the thread
        if local_spawn{
            task
                .into_actor(self)
                .spawn(ctx);
        } else{
            spawn(task);
        }
        
    }

}

/* **************************************************************************************** */
/* ***************************** REDIS CONSUMER NOTIF HANDLER ***************************** */
/* **************************************************************************************** */
impl ActixMessageHandler<ConsumeNotifFromRedis> for NotifBrokerActor{
    
    type Result = ();
    
    fn handle(&mut self, msg: ConsumeNotifFromRedis, ctx: &mut Self::Context) -> Self::Result {

        let ConsumeNotifFromRedis { channel } = msg.clone();
        let this = self.clone();

        let task = async move{

            // must store in db (mutator actor)
            // cache on redis 
            // zerlog producer
            // send to ws mpsc notif sender

        };

        // spawn the task in the background ligh thread 
        // don't await on the spawn; let the task execute 
        // in the background unless you want to use select
        // or tell the runtime someone needs the result right
        // now but notify later once the task is completed 
        // and don't block the thread
        spawn(task);
    }
}

/* **************************************************************************************** */
/* ***************************** KAFKA PRODUCER NOTIF HANDLER ***************************** */
/* **************************************************************************************** */
impl ActixMessageHandler<PublishNotifToKafka> for NotifBrokerActor{
    
    type Result = ();
    
    fn handle(&mut self, msg: PublishNotifToKafka, ctx: &mut Self::Context) -> Self::Result {

        
    }
}

/* **************************************************************************************** */
/* ***************************** KAFKA CONSUMER NOTIF HANDLER ***************************** */
/* **************************************************************************************** */
impl ActixMessageHandler<ConsumeNotifFromKafka> for NotifBrokerActor{
    
    type Result = ();
    
    fn handle(&mut self, msg: ConsumeNotifFromKafka, ctx: &mut Self::Context) -> Self::Result {
        
    }
}
